use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::runtime;

use crate::local::{choose_local_tracks, collect_tracks, play_path, play_tracks};
use crate::telegram::{
    SESSION_FILE, TELEGRAM_CREDENTIALS_FILE, TelegramCatalogEntry, channel_catalog,
    choose_catalog_tracks, download_channel, play_catalog_entries, stream_channel, sync_catalog,
    telegram_client,
};
use crate::util::{LIBRARY_FILE, PlayerExit, clear_screen, normalize_channel, prompt, select_menu};
const PLAYLIST_FILE: &str = ".music-terminal-playlists";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Playlist {
    pub(crate) name: String,
    pub(crate) channel: String,
    pub(crate) tracks: Vec<TelegramCatalogEntry>,
    pub(crate) local_tracks: Vec<PathBuf>,
}

impl Playlist {
    fn is_local(&self) -> bool {
        !self.local_tracks.is_empty() || self.channel.is_empty()
    }

    fn song_count(&self) -> usize {
        if self.is_local() {
            self.local_tracks.len()
        } else {
            self.tracks.len()
        }
    }
}

#[derive(Default)]
struct SavedLibrary {
    channels: Vec<String>,
    folders: Vec<PathBuf>,
}

pub(crate) fn launch_menu() -> Result<()> {
    let mut library = load_library()?;
    let items = vec![
        "Play shuffle (Telegram)".to_string(),
        "Play local folder".to_string(),
        "Stream Telegram channel".to_string(),
        "Update Telegram song list".to_string(),
        "Download Telegram channel to .\\music".to_string(),
        "Playlists".to_string(),
        "Login to Telegram".to_string(),
        "Quit".to_string(),
    ];
    loop {
        let title = format!(
            "Music Terminal Player\nTelegram: {}",
            telegram_login_status()
        );
        match select_menu(&title, &items)? {
            Some(0) => {
                if let Some(channel) = choose_channel(&mut library)? {
                    let catalog = channel_catalog(&channel)?;
                    let exit = runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(play_catalog_entries(&channel, catalog, 0, true))?;
                    if exit == PlayerExit::Quit {
                        return Ok(());
                    }
                }
            }
            Some(1) => {
                if let Some(path) = choose_local_folder(&mut library)?
                    && play_path(&path)? == PlayerExit::Quit
                {
                    return Ok(());
                }
            }
            Some(2) => {
                if let Some(channel) = choose_channel(&mut library)? {
                    let exit = runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(stream_channel(&channel))?;
                    if exit == PlayerExit::Quit {
                        return Ok(());
                    }
                }
            }
            Some(3) => {
                if let Some(channel) = choose_channel(&mut library)? {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(sync_catalog(&channel))?;
                    prompt("Press Enter to return to the menu...")?;
                }
            }
            Some(4) => {
                if let Some(channel) = choose_channel(&mut library)? {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(download_channel(&channel, Path::new("music")))?;
                }
            }
            Some(5) => {
                if manage_playlists(&mut library)? == PlayerExit::Quit {
                    return Ok(());
                }
            }
            Some(6) => runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    telegram_client().await?;
                    println!("Telegram account ready.");
                    Ok::<(), anyhow::Error>(())
                })?,
            Some(7) | None => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn load_library() -> Result<SavedLibrary> {
    let mut library = SavedLibrary::default();
    let text = match fs::read_to_string(LIBRARY_FILE) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(library),
        Err(err) => return Err(err.into()),
    };
    for line in text.lines() {
        if let Some(channel) = line.strip_prefix("channel\t") {
            library.channels.push(channel.to_string());
        } else if let Some(folder) = line.strip_prefix("folder\t") {
            library.folders.push(PathBuf::from(folder));
        }
    }
    Ok(library)
}

fn save_library(library: &SavedLibrary) -> Result<()> {
    let mut text = String::new();
    for channel in &library.channels {
        text.push_str(&format!("channel\t{channel}\n"));
    }
    for folder in &library.folders {
        text.push_str(&format!("folder\t{}\n", folder.display()));
    }
    fs::write(LIBRARY_FILE, text)?;
    Ok(())
}

fn choose_channel(library: &mut SavedLibrary) -> Result<Option<String>> {
    loop {
        let mut items: Vec<_> = library
            .channels
            .iter()
            .map(|channel| format!("@{channel}"))
            .collect();
        let channel_count = items.len();
        items.extend([
            "Add channel".to_string(),
            "Forget channel".to_string(),
            "Back".to_string(),
        ]);
        match select_menu("Telegram channels", &items)? {
            Some(index) if index < channel_count => {
                return Ok(Some(library.channels[index].clone()));
            }
            Some(index) if index == channel_count => {
                clear_screen()?;
                let channel = normalize_channel(&prompt("Public channel: ")?);
                if !channel.is_empty() && !library.channels.contains(&channel) {
                    library.channels.push(channel);
                    save_library(library)?;
                }
            }
            Some(index) if index == channel_count + 1 => {
                forget_channel(library)?;
            }
            Some(_) | None => return Ok(None),
        }
    }
}

fn forget_channel(library: &mut SavedLibrary) -> Result<()> {
    let mut items: Vec<_> = library
        .channels
        .iter()
        .map(|channel| format!("@{channel}"))
        .collect();
    let channel_count = items.len();
    items.push("Cancel".to_string());
    if let Some(index) = select_menu("Forget which channel?", &items)?
        && index < channel_count
    {
        library.channels.remove(index);
        save_library(library)?;
    }
    Ok(())
}

fn choose_local_folder(library: &mut SavedLibrary) -> Result<Option<PathBuf>> {
    loop {
        let mut items: Vec<_> = library
            .folders
            .iter()
            .map(|folder| folder.display().to_string())
            .collect();
        let folder_count = items.len();
        items.extend([
            "Add folder".to_string(),
            "Forget location".to_string(),
            "Back".to_string(),
        ]);
        match select_menu("Local folders", &items)? {
            Some(index) if index < folder_count => {
                return Ok(Some(library.folders[index].clone()));
            }
            Some(index) if index == folder_count => {
                clear_screen()?;
                let input = prompt("Folder path: ")?;
                let path = fs::canonicalize(input.trim())
                    .with_context(|| format!("folder not found: {}", input.trim()))?;
                if !path.is_dir() {
                    bail!("not a folder: {}", path.display());
                }
                if !library.folders.contains(&path) {
                    library.folders.push(path);
                    save_library(library)?;
                }
            }
            Some(index) if index == folder_count + 1 => {
                forget_folder(library)?;
            }
            Some(_) | None => return Ok(None),
        }
    }
}

fn forget_folder(library: &mut SavedLibrary) -> Result<()> {
    let mut items: Vec<_> = library
        .folders
        .iter()
        .map(|folder| folder.display().to_string())
        .collect();
    let folder_count = items.len();
    items.push("Cancel".to_string());
    if let Some(index) = select_menu("Forget which location?", &items)?
        && index < folder_count
    {
        library.folders.remove(index);
        save_library(library)?;
    }
    Ok(())
}

fn manage_playlists(library: &mut SavedLibrary) -> Result<PlayerExit> {
    let mut playlists = load_playlists()?;
    loop {
        let mut items: Vec<_> = playlists
            .iter()
            .map(|playlist| {
                let source = if playlist.is_local() {
                    "local".to_string()
                } else {
                    format!("@{}", playlist.channel)
                };
                format!(
                    "{} ({source}, {} songs)",
                    playlist.name,
                    playlist.song_count()
                )
            })
            .collect();
        let playlist_count = items.len();
        items.extend([
            "Create playlist".to_string(),
            "Edit playlist songs".to_string(),
            "Rename playlist".to_string(),
            "Delete playlist".to_string(),
            "Back".to_string(),
        ]);
        match select_menu("Playlists", &items)? {
            Some(index) if index < playlist_count => {
                let playlist = playlists[index].clone();
                let exit = if playlist.is_local() {
                    play_tracks(playlist.local_tracks)?
                } else {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(play_catalog_entries(
                            &playlist.channel,
                            playlist.tracks,
                            0,
                            false,
                        ))?
                };
                if exit == PlayerExit::Quit {
                    return Ok(exit);
                }
            }
            Some(index) if index == playlist_count => {
                create_playlist(library, &mut playlists)?;
            }
            Some(index) if index == playlist_count + 1 => {
                if let Some(index) = choose_playlist("Edit which playlist?", &playlists)? {
                    edit_playlist(library, &mut playlists[index])?;
                    save_playlists(&playlists)?;
                }
            }
            Some(index) if index == playlist_count + 2 => {
                if let Some(index) = choose_playlist("Rename which playlist?", &playlists)? {
                    clear_screen()?;
                    let name = clean_playlist_name(&prompt("New playlist name: ")?);
                    if !name.is_empty() {
                        playlists[index].name = name;
                        save_playlists(&playlists)?;
                    }
                }
            }
            Some(index) if index == playlist_count + 3 => {
                if let Some(index) = choose_playlist("Delete which playlist?", &playlists)? {
                    playlists.remove(index);
                    save_playlists(&playlists)?;
                }
            }
            Some(_) | None => return Ok(PlayerExit::Back),
        }
    }
}

fn create_playlist(library: &mut SavedLibrary, playlists: &mut Vec<Playlist>) -> Result<()> {
    clear_screen()?;
    let name = clean_playlist_name(&prompt("Playlist name: ")?);
    if name.is_empty() {
        return Ok(());
    }
    let Some(source) = select_menu(
        "Playlist source",
        &["Telegram channel".to_string(), "Local folder".to_string()],
    )?
    else {
        return Ok(());
    };
    let mut playlist = Playlist {
        name,
        channel: String::new(),
        tracks: Vec::new(),
        local_tracks: Vec::new(),
    };
    if source == 0 {
        edit_telegram_playlist(library, &mut playlist)?;
    } else {
        edit_local_playlist(library, &mut playlist)?;
    }
    if playlist.song_count() > 0 {
        playlists.push(playlist);
        save_playlists(playlists)?;
    }
    Ok(())
}

// ponytail: one source per playlist; add a unified local/Telegram playback queue for mixed-source lists.
fn edit_playlist(library: &mut SavedLibrary, playlist: &mut Playlist) -> Result<()> {
    let Some(source) = select_menu(
        "Playlist source",
        &["Telegram channel".to_string(), "Local folder".to_string()],
    )?
    else {
        return Ok(());
    };
    if source == 0 {
        edit_telegram_playlist(library, playlist)
    } else {
        edit_local_playlist(library, playlist)
    }
}

fn edit_telegram_playlist(library: &mut SavedLibrary, playlist: &mut Playlist) -> Result<()> {
    let Some(channel) = choose_channel(library)? else {
        return Ok(());
    };
    let catalog = channel_catalog(&channel)?;
    let selected_ids: Vec<_> = if channel == playlist.channel {
        playlist
            .tracks
            .iter()
            .map(|track| track.message_id)
            .collect()
    } else {
        Vec::new()
    };
    if let Some(tracks) = choose_catalog_tracks(&catalog, &selected_ids)? {
        playlist.channel = channel;
        playlist.tracks = tracks;
        playlist.local_tracks.clear();
    }
    Ok(())
}

fn edit_local_playlist(library: &mut SavedLibrary, playlist: &mut Playlist) -> Result<()> {
    let Some(folder) = choose_local_folder(library)? else {
        return Ok(());
    };
    let tracks = collect_tracks(&folder)?;
    if let Some(selected) = choose_local_tracks(&tracks, &playlist.local_tracks)? {
        playlist.channel.clear();
        playlist.tracks.clear();
        playlist.local_tracks = selected;
    }
    Ok(())
}

fn choose_playlist(title: &str, playlists: &[Playlist]) -> Result<Option<usize>> {
    if playlists.is_empty() {
        return Ok(None);
    }
    let mut items: Vec<_> = playlists
        .iter()
        .map(|playlist| playlist.name.clone())
        .collect();
    let count = items.len();
    items.push("Cancel".to_string());
    Ok(select_menu(title, &items)?.filter(|index| *index < count))
}

fn clean_playlist_name(name: &str) -> String {
    name.trim().replace(['\t', '\r', '\n'], " ")
}

fn load_playlists() -> Result<Vec<Playlist>> {
    let text = match fs::read_to_string(PLAYLIST_FILE) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    Ok(parse_playlists(&text))
}

pub(crate) fn parse_playlists(text: &str) -> Vec<Playlist> {
    let mut playlists: Vec<Playlist> = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(4, '\t');
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("playlist"), Some(name), Some(channel), _) => playlists.push(Playlist {
                name: name.to_string(),
                channel: channel.to_string(),
                tracks: Vec::new(),
                local_tracks: Vec::new(),
            }),
            (Some("track"), Some(message_id), Some(name), _rest) => {
                if let Some(playlist) = playlists.last_mut()
                    && let Ok(message_id) = message_id.parse()
                {
                    playlist.tracks.push(TelegramCatalogEntry {
                        message_id,
                        name: name.to_string(),
                    });
                }
            }
            (Some("local"), Some(path), _, _) => {
                if let Some(playlist) = playlists.last_mut() {
                    playlist.local_tracks.push(PathBuf::from(path));
                }
            }
            _ => {}
        }
    }
    playlists
}

fn save_playlists(playlists: &[Playlist]) -> Result<()> {
    fs::write(PLAYLIST_FILE, serialize_playlists(playlists))?;
    Ok(())
}

pub(crate) fn serialize_playlists(playlists: &[Playlist]) -> String {
    let mut text = String::new();
    for playlist in playlists {
        text.push_str(&format!(
            "playlist\t{}\t{}\n",
            clean_playlist_name(&playlist.name),
            playlist.channel
        ));
        for track in &playlist.tracks {
            text.push_str(&format!(
                "track\t{}\t{}\n",
                track.message_id,
                track.name.replace(['\t', '\r', '\n'], " ")
            ));
        }
        for track in &playlist.local_tracks {
            text.push_str(&format!(
                "local\t{}\n",
                track.to_string_lossy().replace(['\t', '\r', '\n'], " ")
            ));
        }
    }
    text
}

fn telegram_login_status() -> &'static str {
    if Path::new(SESSION_FILE).exists() && Path::new(TELEGRAM_CREDENTIALS_FILE).exists() {
        "logged in (saved session)"
    } else {
        "not logged in"
    }
}

pub(crate) fn print_usage() {
    println!("music-terminal-player");
    println!();
    println!("Usage:");
    println!("  music-terminal-player play <file-or-folder>");
    println!("  music-terminal-player login");
    println!("  music-terminal-player stream <public-channel>");
    println!("  music-terminal-player sync <public-channel>");
    println!("  music-terminal-player download <public-channel> [folder]");
    println!();
    println!("Controls while playing:");
    println!(
        "  p play/pause | n next | v previous | l loop mode | Left/Right seek | +/- volume | q quit"
    );
    println!();
    println!("Telegram login and streaming need TG_ID and TG_HASH from https://my.telegram.org");
}
