use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::runtime;

mod app;
mod local;
mod telegram;
#[cfg(test)]
mod tests;
mod util;

fn main() {
    let launched_without_args = env::args_os().len() == 1;
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        if launched_without_args {
            let _ = util::prompt("Press Enter to close...");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("play") => {
            let path = args
                .next()
                .context("missing path: music-terminal-player play <file-or-folder>")?;
            local::play_path(Path::new(&path)).map(|_| ())
        }
        Some("login") => runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async {
                telegram::telegram_client().await?;
                println!("Telegram account ready.");
                Ok::<(), anyhow::Error>(())
            }),
        Some("stream") => {
            let channel = args
                .next()
                .context("missing channel: music-terminal-player stream <public-channel>")?;
            runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(telegram::stream_channel(&channel))
                .map(|_| ())
        }
        Some("sync") => {
            let channel = args
                .next()
                .context("missing channel: music-terminal-player sync <public-channel>")?;
            runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(telegram::sync_catalog(&channel))
        }
        Some("download") => {
            let channel = args.next().context(
                "missing channel: music-terminal-player download <public-channel> [folder]",
            )?;
            let folder = args.next().unwrap_or_else(|| "music".to_string());
            runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(telegram::download_channel(&channel, Path::new(&folder)))
        }
        Some("help") | Some("--help") | Some("-h") => {
            app::print_usage();
            Ok(())
        }
        None => app::launch_menu(),
        Some(other) => bail!("unknown command: {other}"),
    }
}
