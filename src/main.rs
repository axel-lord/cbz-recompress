//! Application entrypoint.

use ::cbz_recompress::Cli;
use ::clap::Parser;
use ::mimalloc::MiMalloc;

/// Global allocator in use.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> ::color_eyre::Result<()> {
    ::color_eyre::install()?;
    ::env_logger::builder()
        .filter_module("cbz_recompress", ::log::LevelFilter::Info)
        .init();
    ::jxl_image_rs_integration::register_image_decoding_hook();

    Cli::parse().run()
}
