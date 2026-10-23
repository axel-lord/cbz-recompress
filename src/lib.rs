//! Recompress cbz archives, lowering resolution and convering
//! images to jpegxl.

use ::core::num::NonZero;
use ::std::{
    fs::File,
    io::{BufReader, Cursor, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use ::color_eyre::{Section, eyre::eyre};
use ::image::{ImageReader, imageops::FilterType};
use ::indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use ::jpegxl_rs::{ResizableRunner, parallel::ParallelRunner};
use ::rayon::iter::{IntoParallelIterator, ParallelIterator};
use ::regex::{Regex, RegexBuilder};
use ::rustix::fs::{AtFlags, Mode, OFlags};
use ::tap::{Conv, Pipe, TryConv};
use ::zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

/// Progress bar template.
const TEMPLATE: &str = "[{elapsed_precise}] {bar:60} {pos:>4}/{len:4} {msg}";

/// Pattern for images.
static IMAGE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"^(.*?)\.(?:png|jpe?g|jxl)")
        .case_insensitive(true)
        .build()
        .expect("regex should compile")
});

/// Default amount of threads available.
fn default_thread_count() -> NonZero<usize> {
    ::std::thread::available_parallelism().unwrap_or(const { NonZero::new(1).unwrap() })
}

/// Recompress cbz archives, lowering resolution and converting to JpegXL.
#[derive(Debug, ::clap::Parser)]
#[command(author, version)]
pub struct Cli {
    /// If inputs are directories, recursively find archives at them.
    #[arg(long, short)]
    recursive: bool,
    /// If recursive, do not ignore hidden files.
    #[arg(long, short = 'I')]
    no_ignore: bool,
    /// Output directory.
    #[arg(long, short)]
    out_dir: PathBuf,
    /// File to write path of successfully recompressed archives to.
    #[arg(long, short)]
    success_file: Option<PathBuf>,
    /// File to write path of archives which failed to recompress to.
    #[arg(long, short)]
    failure_file: Option<PathBuf>,
    /// Max width of archive images.
    #[arg(long, short = 'W', default_value_t = 1920)]
    max_width: u32,
    /// Max height of archive images.
    #[arg(long, short = 'H', default_value_t = 1920)]
    max_height: u32,
    /// Print names all archives to stdout before any recompression.
    #[arg(long, short)]
    verbose: bool,
    /// Do not recompress any archives.
    #[arg(long, short)]
    dry_run: bool,
    /// When printing names using verbose terminate using null character.
    #[arg(long, short = '0')]
    print0: bool,
    /// How many threads to use for parallel recompressing.
    #[arg(long, short, default_value_t=default_thread_count())]
    threads: NonZero<usize>,
    /// Maximum pages to recompress. If zero do not limit.
    #[arg(long, short, default_value_t = 0)]
    max_pages: usize,
    /// Cbz archive/s to recompress.
    archive: Vec<PathBuf>,
}

impl Cli {
    /// Run cli.
    pub fn run(self) -> ::color_eyre::Result<()> {
        let Self {
            recursive,
            no_ignore,
            out_dir,
            archive,
            max_width,
            max_height,
            success_file,
            failure_file,
            verbose,
            dry_run,
            print0,
            threads,
            max_pages,
        } = self;

        let max_pages = if max_pages == 0 {
            usize::MAX
        } else {
            max_pages
        };

        ::rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            .build_global()?;

        let success_file = if let Some(success_file) = &success_file {
            let file = File::create(success_file).map_err(|err| {
                eyre!("Could not create success file {:?}", success_file.display()).error(err)
            })?;
            Some(Mutex::new(file))
        } else {
            None
        };

        let failure_file = if let Some(failure_file) = &failure_file {
            let file = File::create(failure_file).map_err(|err| {
                eyre!("Could not create failure file {:?}", failure_file.display()).error(err)
            })?;
            Some(Mutex::new(file))
        } else {
            None
        };

        ::std::fs::create_dir_all(&out_dir).map_err(|err| {
            eyre!("could not create output directory {:?}", out_dir.display()).error(err)
        })?;
        let out_dir = &out_dir.canonicalize().map_err(|err| {
            eyre!("Could not canonicalize output dir {:}", out_dir.display()).error(err)
        })?;

        let mut archive_entries;
        let archive = if recursive {
            archive_entries = Vec::new();
            let mut archives = Vec::new();

            for archive in &archive {
                if !archive.is_dir() {
                    archives.push(Archive::use_parent(archive));
                    continue;
                }

                let walker = ::walkdir::WalkDir::new(archive)
                    .same_file_system(true)
                    .into_iter()
                    .filter_entry(|e| {
                        let a = e
                            .path()
                            .canonicalize()
                            .map(|epath| &epath != out_dir)
                            .unwrap_or(true);

                        let b = no_ignore || !e.file_name().to_str().unwrap_or("").starts_with('.');
                        a && b
                    });

                for entry in walker {
                    let entry = entry.map_err(|err| {
                        eyre!("could not get an entry walking {:?}", archive.display()).error(err)
                    })?;
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let file_name = entry.file_name().as_bytes();

                    let ext = entry
                        .file_name()
                        .as_bytes()
                        .get((file_name.len().saturating_sub(4))..);
                    let Some(ext) = ext else {
                        continue;
                    };

                    if !ext.eq_ignore_ascii_case(b".cbz") {
                        continue;
                    }

                    archive_entries.push((archive, entry));
                }
            }

            for (dir, entry) in &archive_entries {
                archives.push(Archive {
                    base_dir: BaseDir::Directory(dir),
                    archive: entry.path(),
                });
            }

            archives
        } else {
            archive
                .iter()
                .map(|a| Archive::use_parent(a))
                .collect::<Vec<_>>()
        };

        if verbose {
            let mut stdout = ::std::io::stdout().lock();
            for Archive { archive, .. } in &archive {
                stdout.write_all(archive.as_os_str().as_bytes())?;
                stdout.write_all(if print0 { b"\0" } else { b"\n" })?;
            }
            stdout.flush()?;
        }

        if dry_run {
            return Ok(());
        }

        let multi_progress = &MultiProgress::new();
        let progress_style = &ProgressStyle::with_template(TEMPLATE)
            .map_err(|err| {
                eyre!("could not create progress style")
                    .note(format!("Template {TEMPLATE:?}"))
                    .error(err)
            })?
            .progress_chars("##-");

        let total = &multi_progress.add(
            ProgressBar::new(archive.len().try_conv::<u64>()?)
                .with_style(progress_style.clone())
                .with_message("Total"),
        );

        total.tick();

        archive.into_par_iter().for_each(|archive| {
            thread_local! {
                static RUNNER: ResizableRunner<'static> = ResizableRunner::default();
            }
            RUNNER.with(|runner| {
                let opts = RecompOpts {
                    archive,
                    out_dir,
                    max_width,
                    max_height,
                    multi_progress,
                    progress_style,
                    runner,
                    total,
                    max_pages,
                };
                let result = opts.recompress();
                total.inc(1);
                if let Err(err) = result {
                    ::log::error!("\n{err:?}");
                    if let Some(file) = &failure_file
                        && let Ok(mut file) = file.lock()
                    {
                        _ = file.write_all(archive.archive.as_os_str().as_bytes());
                        _ = file.write_all(b"\n");
                    }
                } else {
                    if let Some(file) = &success_file
                        && let Ok(mut file) = file.lock()
                    {
                        _ = file.write_all(archive.archive.as_os_str().as_bytes());
                        _ = file.write_all(b"\n");
                    }
                }
            });
        });
        total.finish();
        Ok(())
    }
}

/// Base directory of archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BaseDir<'a> {
    /// Use parent as base directory.
    Parent,
    /// Use specified directory.
    Directory(&'a Path),
}

/// A single archive, with a base directory.
#[derive(Debug, Clone, Copy)]
struct Archive<'a> {
    /// Base directory of archive.
    base_dir: BaseDir<'a>,
    /// Path to archive.
    archive: &'a Path,
}

impl<'a> Archive<'a> {
    /// Create a new archive path using parent for base dir.
    const fn use_parent(archive: &'a Path) -> Self {
        Self {
            base_dir: BaseDir::Parent,
            archive,
        }
    }

    /// Get archive and base dir
    fn archive_base_dir(self) -> ::color_eyre::Result<(&'a Path, &'a Path)> {
        let Self { base_dir, archive } = self;

        Ok((
            archive,
            match base_dir {
                BaseDir::Parent => {
                    if let Some(parent) = archive.parent() {
                        parent
                    } else {
                        return Err(eyre!("Could not get parent of archive")
                            .note(format!("Archive {}", archive.display())));
                    }
                }
                BaseDir::Directory(path) => path,
            },
        ))
    }
}

/// Options given to recompress archive.
#[derive(Clone, Copy)]
struct RecompOpts<'a> {
    /// Archive to recompress.
    archive: Archive<'a>,
    /// Output directory.
    out_dir: &'a Path,
    /// Max width of images.
    max_width: u32,
    /// Max height of images.
    max_height: u32,
    /// Style of progress bar.
    progress_style: &'a ProgressStyle,
    /// Multiprogress state.
    multi_progress: &'a MultiProgress,
    /// Total progress bar.
    total: &'a ProgressBar,
    /// Encoder runner.
    runner: &'a dyn ParallelRunner,
    /// Maximum pages to recompress.
    max_pages: usize,
}

impl RecompOpts<'_> {
    /// Recompress archive.
    fn recompress(self) -> ::color_eyre::Result<()> {
        let Self {
            archive,
            out_dir,
            max_width,
            max_height,
            progress_style,
            multi_progress,
            runner,
            total,
            max_pages,
        } = self;
        let mut encoder = ::jpegxl_rs::encoder_builder()
            .parallel_runner(runner)
            .build()
            .map_err(|err| eyre!("could not build jxl encoder").error(err))?;

        let (archive, base_dir) = archive.archive_base_dir()?;

        let relative_name = archive.strip_prefix(base_dir).map_err(|err| {
            eyre!(
                "Could not get relative name for archive {:?}, by removing prefix {:?}",
                archive.display(),
                base_dir.display()
            )
            .error(err)
        })?;
        let new_path = out_dir.join(relative_name);

        let new_path_parent = new_path.parent().ok_or_else(|| {
            eyre!("Could not get parent of {:?}", new_path.display())
                .note(format!("Used for archive {:?}", archive.display()))
        })?;

        ::std::fs::create_dir_all(new_path_parent).map_err(|err| {
            eyre!(
                "Could not create output archive parent {:?}",
                new_path_parent.display()
            )
            .note(format!("Used for archive {:?}", archive.display()))
            .error(err)
        })?;

        let mut infile = File::open(archive)
            .map_err(|err| eyre!("Could not open archive {:?}", archive.display()).error(err))?
            .pipe(BufReader::new)
            .pipe(ZipArchive::new)
            .map_err(|err| {
                eyre!(
                    "Could not read archive {:?} as a zip file",
                    archive.display()
                )
                .error(err)
            })?;

        let mut outfile = ::rustix::fs::open(
            out_dir,
            OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        )
        .map_err(|err| {
            eyre!("Could not create unlinked file in {:?}", out_dir.display())
                .note(format!("Used for recompress of {:?}", archive.display()))
                .error(err)
        })?
        .conv::<File>()
        .pipe(ZipWriter::new);

        let progress = multi_progress.insert_before(
            total,
            ProgressBar::new(infile.len().try_conv::<u64>().map_err(|err| {
                eyre!(
                    "input archive {:?} has more files than would fit in a u64, {}",
                    archive.display(),
                    infile.len()
                )
                .error(err)
            })?)
            .with_message(if let Some(name) = archive.file_name() {
                name.display().to_string()
            } else {
                archive.display().to_string()
            })
            .with_style(progress_style.clone()),
        );

        let mut buf = Vec::new();
        for idx in 0..(infile.len().min(max_pages)) {
            let mut file = infile.by_index(idx).map_err(|err| {
                eyre!(
                    "Could not get file in zip at index {idx} in archive {:?}",
                    archive.display()
                )
                .error(err)
            })?;
            let name = String::from_utf8_lossy(file.name_raw()).into_owned();
            let options = FileOptions::DEFAULT
                .last_modified_time(::zip::DateTime::default_for_write())
                .compression_method(::zip::CompressionMethod::Deflated)
                .compression_level(Some(9));

            if file.is_dir() {
                outfile
                    .add_directory(name.clone(), options)
                    .map_err(|err| {
                        eyre!(
                            "Could not add directory {name:?} to output archive of input archive {:?}",
                            archive.display()
                        )
                        .error(err)
                    })?;
                progress.inc(1);
                continue;
            }

            let Some(captures) = IMAGE_PATTERN.captures(&name) else {
                outfile.start_file(name.clone(), options).map_err(|err| {
                    eyre!(
                        "Could not start file {name:?} in output archive of input archive {:?}",
                        archive.display()
                    )
                    .error(err)
                })?;
                ::std::io::copy(&mut file, &mut outfile).map_err(|err| {
                    eyre!(
                        "could not copy {name:?} to output archive of input archive {:?}",
                        archive.display()
                    )
                    .error(err)
                })?;
                progress.inc(1);
                continue;
            };

            let jxl_name = format!(
                "{}.jxl",
                captures
                    .get(1)
                    .ok_or_else(|| eyre!(
                        "could not get group 1 of regex captures for {name:?} in {:?}",
                        archive.display()
                    ))?
                    .as_str()
            );
            outfile
                .start_file(
                    &jxl_name,
                    options
                        .compression_level(None)
                        .compression_method(CompressionMethod::Stored),
                )
                .map_err(|err| {
                    eyre!(
                        "Could not start file {name:?} in output archive of input archive {:?}",
                        archive.display()
                    )
                    .error(err)
                })?;

            buf.clear();
            file.read_to_end(&mut buf).map_err(|err| {
                eyre!(
                    "could not read {name:?} from archive {:?}",
                    archive.display()
                )
                .error(err)
            })?;
            let mut image = Cursor::new(&mut buf)
                .pipe(ImageReader::new)
                .with_guessed_format()
                .map_err(|err| {
                    eyre!(
                        "could not guess format of {name:?} in {:?}",
                        archive.display()
                    )
                    .error(err)
                })?
                .decode()
                .map_err(|err| {
                    eyre!("could not decode {name:?} in {:?}", archive.display()).error(err)
                })?;

            if image.width() > max_width || image.height() > max_height {
                image = image.resize(max_width, max_height, FilterType::Lanczos3);
            }

            let image = image.into_rgb16();
            let encoded = encoder
                .encode(&image, image.width(), image.height())
                .map_err(|err| {
                    eyre!(
                        "could not re-encode {name:?} from {:?} as jxl",
                        archive.display()
                    )
                    .error(err)
                })?;

            outfile.write_all(&encoded).map_err(|err| {
                eyre!("could not write {jxl_name:?} to output archive, from {name:?} in input archvie {:?}", archive.display())
                    .error(err)
            })?;
            progress.inc(1);
        }
        progress.finish();

        let outfile = outfile.finish().map_err(|err| {
            eyre!(
                "Could not finish output archive of input archive {:?}",
                archive.display()
            )
            .error(err)
        })?;

        if ::std::fs::remove_file(&new_path).is_ok() {
            ::log::warn!("Removed existing file at {:?}", new_path.display());
        };

        ::rustix::fs::linkat(
            &outfile,
            "",
            ::rustix::fs::CWD,
            &new_path,
            AtFlags::EMPTY_PATH,
        )
        .map_err(|err| {
            eyre!(
                "Could not link output archive to {:?}, for input archive {:?}",
                new_path.display(),
                archive.display()
            )
            .error(err)
        })?;

        Ok(())
    }
}
