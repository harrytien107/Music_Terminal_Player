use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::runtime;

mod app;
mod audio_output;
mod i18n;
mod local;
mod media_controls;
mod telegram;
#[cfg(test)]
mod tests;
mod util;
mod youtube;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let borderless = args.iter().any(|arg| arg == "--borderless");
    let launched_interactively = args.iter().all(|arg| arg == "--borderless");
    if borderless {
        configure_player_window();
    }
    if let Err(error) = run(args.into_iter().filter(|arg| arg != "--borderless")) {
        eprintln!("Error: {error:#}");
        if launched_interactively {
            let _ = util::prompt("Press Enter to close...");
        }
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn configure_player_window() {
    use std::ffi::c_void;

    // Windows Terminal owns its top-level window, so a console app cannot safely restyle its tab.
    if env::var_os("WT_SESSION").is_some() {
        return;
    }

    type Hwnd = *mut c_void;
    const GWL_STYLE: i32 = -16;
    const WS_OVERLAPPEDWINDOW: i32 = 0x00cf_0000;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_FRAMECHANGED: u32 = 0x0020;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> Hwnd;
    }
    #[link(name = "User32")]
    unsafe extern "system" {
        fn GetWindowLongW(window: Hwnd, index: i32) -> i32;
        fn SetWindowLongW(window: Hwnd, index: i32, value: i32) -> i32;
        fn SetWindowPos(
            window: Hwnd,
            insert_after: Hwnd,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
    }

    // SAFETY: The handle comes from the current process console. Calls are skipped without one.
    unsafe {
        let window = GetConsoleWindow();
        if window.is_null() {
            return;
        }
        let style = GetWindowLongW(window, GWL_STYLE);
        SetWindowLongW(window, GWL_STYLE, style & !WS_OVERLAPPEDWINDOW);
        SetWindowPos(
            window,
            std::ptr::null_mut(),
            0,
            0,
            1200,
            500,
            SWP_NOMOVE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

#[cfg(not(windows))]
fn configure_player_window() {}

fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    util::prepare_data_dir()?;
    i18n::load_language();
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
        Some("youtube") => youtube::play_youtube().map(|_| ()),
        Some("help") | Some("--help") | Some("-h") => {
            app::print_usage();
            Ok(())
        }
        None => app::launch_menu(),
        Some(other) => bail!("unknown command: {other}"),
    }
}
