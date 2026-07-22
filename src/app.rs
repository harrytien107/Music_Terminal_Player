use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::runtime;

use crate::local::{
    choose_local_tracks, collect_tracks, play_path, play_tracks, play_tracks_shuffled,
};
use crate::telegram::{
    SESSION_FILE, TELEGRAM_CREDENTIALS_FILE, TelegramCatalogEntry, channel_catalog,
    choose_catalog_tracks, download_channel, play_catalog_entries, stream_catalog_entries,
    sync_catalog, telegram_client,
};
use crate::util::{
    LIBRARY_FILE, PLAYLIST_FILE, PlayerExit, clear_screen, normalize_channel, prompt, select_menu,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Playlist {
    pub(crate) name: String,
    pub(crate) channel: String,
    pub(crate) tracks: Vec<TelegramCatalogEntry>,
    pub(crate) local_tracks: Vec<PathBuf>,
}

impl Playlist {
    fn is_local(&self) -> bool {
        !self.local_tracks.is_empty() || self.tracks.is_empty()
    }

    fn song_count(&self) -> usize {
        if self.is_local() {
            self.local_tracks.len()
        } else {
            self.tracks.len()
        }
    }

    fn channel_count(&self) -> usize {
        self.tracks
            .iter()
            .map(|track| track.channel.as_str())
            .collect::<HashSet<_>>()
            .len()
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
        "▶  Quick play".to_string(),
        "♫  Play local folder".to_string(),
        "☁  Stream Telegram channel".to_string(),
        "↻  Sync and update Telegram channel".to_string(),
        "↓  Download Telegram channel to .\\music".to_string(),
        "▶  Play YouTube audio".to_string(),
        "≡  Open playlists".to_string(),
        "●  Login to Telegram".to_string(),
        "×  Quit".to_string(),
    ];
    loop {
        let title = format!(
            "╭──────────────────────────────────────╮\n\
             │        MUSIC TERMINAL PLAYER         │\n\
             ╰──────────────────────────────────────╯\n\
             Telegram · {}",
            telegram_login_status()
        );
        match select_menu(&title, &items)? {
            Some(0) => {
                if quick_play_menu(&library)? == PlayerExit::Quit {
                    return Ok(());
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
                if let Some((label, catalog)) =
                    choose_telegram_catalog(&library, "Stream Telegram channel")?
                {
                    let result = runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(stream_catalog_entries(&label, catalog));
                    match result {
                        Ok(PlayerExit::Quit) => return Ok(()),
                        Ok(PlayerExit::Back) => {}
                        Err(error) => show_menu_error(&error)?,
                    }
                }
            }
            Some(3) => {
                while let Some((channel, save_after_sync)) = choose_channel_to_sync(&mut library)? {
                    let result = runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(sync_catalog(&channel));
                    match result {
                        Ok(()) => {
                            if save_after_sync && !library.channels.contains(&channel) {
                                library.channels.push(channel);
                                save_library(&library)?;
                            }
                            prompt("Synchronization complete. Press Enter to continue...")?;
                        }
                        Err(error) => show_menu_error(&error)?,
                    }
                }
            }
            Some(4) => {
                if let Some(channel) =
                    choose_saved_channel(&library, "Download saved Telegram channel")?
                {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(download_channel(&channel, Path::new("music")))?;
                }
            }
            Some(5) => match crate::youtube::play_youtube() {
                Ok(PlayerExit::Quit) => return Ok(()),
                Ok(PlayerExit::Back) => {}
                Err(error) => show_menu_error(&error)?,
            },
            Some(6) => {
                if manage_playlists(&mut library)? == PlayerExit::Quit {
                    return Ok(());
                }
            }
            Some(7) => loop {
                clear_screen()?;
                let result = runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?
                    .block_on(telegram_client());
                match result {
                    Ok(_) => {
                        println!("Telegram account ready.");
                        prompt("Press Enter to return to the menu...")?;
                        break;
                    }
                    Err(error) => {
                        println!("Telegram login failed: {error:#}");
                        let action = prompt("Press Enter to retry, or type 'back': ")?;
                        if action.trim().eq_ignore_ascii_case("back") {
                            break;
                        }
                    }
                }
            },
            Some(8) | None => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn quick_play_menu(library: &SavedLibrary) -> Result<PlayerExit> {
    let items = vec![
        "▶  Shuffle Telegram library".to_string(),
        "♫  Shuffle Local library".to_string(),
        "←  Back".to_string(),
    ];
    loop {
        match select_menu("Quick play", &items)? {
            Some(0) => {
                let result = all_channel_tracks(library).and_then(|catalog| {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(play_catalog_entries("", catalog, 0, true))
                });
                match result {
                    Ok(PlayerExit::Quit) => return Ok(PlayerExit::Quit),
                    Ok(PlayerExit::Back) => {}
                    Err(error) => show_menu_error(&error)?,
                }
            }
            Some(1) => {
                match collect_library_tracks(&library.folders).and_then(play_tracks_shuffled) {
                    Ok(PlayerExit::Quit) => return Ok(PlayerExit::Quit),
                    Ok(PlayerExit::Back) => {}
                    Err(error) => show_menu_error(&error)?,
                }
            }
            Some(2) | None => return Ok(PlayerExit::Back),
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

fn choose_telegram_catalog(
    library: &SavedLibrary,
    title: &str,
) -> Result<Option<(String, Vec<TelegramCatalogEntry>)>> {
    if library.channels.is_empty() {
        clear_screen()?;
        println!("No synchronized Telegram channels.");
        prompt("Use 'Sync and update Telegram channel' to add one. Press Enter to return...")?;
        return Ok(None);
    }
    let mut items = vec!["All synchronized channels".to_string()];
    items.extend(library.channels.iter().map(|channel| format!("@{channel}")));
    items.push("Back".to_string());
    let Some(index) = select_menu(title, &items)? else {
        return Ok(None);
    };
    if index == 0 {
        let tracks = all_channel_tracks(library)?;
        return Ok(Some((
            format!(
                "All synchronized Telegram channels\n{} tracks",
                tracks.len()
            ),
            tracks,
        )));
    }
    if index <= library.channels.len() {
        let channel = &library.channels[index - 1];
        let tracks = channel_catalog(channel)?;
        return Ok(Some((
            format!("Telegram channel: @{channel}\n{} tracks", tracks.len()),
            tracks,
        )));
    }
    Ok(None)
}

fn choose_saved_channel(library: &SavedLibrary, title: &str) -> Result<Option<String>> {
    if library.channels.is_empty() {
        clear_screen()?;
        println!("No synchronized Telegram channels.");
        prompt("Use 'Sync and update Telegram channel' to add one. Press Enter to return...")?;
        return Ok(None);
    }
    let mut items: Vec<_> = library
        .channels
        .iter()
        .map(|channel| format!("@{channel}"))
        .collect();
    let channel_count = items.len();
    items.push("Back".to_string());
    Ok(select_menu(title, &items)?
        .and_then(|index| (index < channel_count).then(|| library.channels[index].clone())))
}

fn choose_channel_to_sync(library: &mut SavedLibrary) -> Result<Option<(String, bool)>> {
    loop {
        let mut items: Vec<_> = library
            .channels
            .iter()
            .map(|channel| format!("Sync and update @{channel}"))
            .collect();
        let channel_count = items.len();
        items.extend([
            "Add and synchronize channel".to_string(),
            "Forget channel".to_string(),
            "Back".to_string(),
        ]);
        let title = "Sync and update Telegram channel\nAdd and synchronize public channels here before streaming.";
        match select_menu(title, &items)? {
            Some(index) if index < channel_count => {
                return Ok(Some((library.channels[index].clone(), false)));
            }
            Some(index) if index == channel_count => {
                clear_screen()?;
                let channel = normalize_channel(&prompt("Public channel: ")?);
                if !channel.is_empty() {
                    return Ok(Some((
                        channel.clone(),
                        !library.channels.contains(&channel),
                    )));
                }
            }
            Some(index) if index == channel_count + 1 => forget_channel(library)?,
            Some(_) | None => return Ok(None),
        }
    }
}

pub(crate) fn collect_library_tracks(folders: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if folders.is_empty() {
        bail!("no saved local folders; add one from Play local folder");
    }
    let mut seen = HashSet::new();
    let mut tracks = Vec::new();
    for folder in folders {
        for track in collect_tracks(folder)? {
            if seen.insert(track.clone()) {
                tracks.push(track);
            }
        }
    }
    if tracks.is_empty() {
        bail!("saved local folders contain no supported audio files");
    }
    Ok(tracks)
}

fn all_channel_tracks(library: &SavedLibrary) -> Result<Vec<TelegramCatalogEntry>> {
    if library.channels.is_empty() {
        bail!("no synchronized Telegram channels; add one from Sync and update Telegram channel");
    }
    let mut tracks = Vec::new();
    for channel in &library.channels {
        tracks.extend(channel_catalog(channel)?);
    }
    if tracks.is_empty() {
        bail!("synchronized Telegram channels contain no tracks");
    }
    Ok(tracks)
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
                    format!("Telegram, {} channels", playlist.channel_count())
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

// ponytail: local and Telegram tracks stay separate; add a source-aware unified queue when mixed playlists are requested.
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
    while let Some(channel) =
        choose_saved_channel(library, "Choose a channel to add or edit playlist songs")?
    {
        let catalog = match channel_catalog(&channel) {
            Ok(catalog) => catalog,
            Err(error) => {
                show_menu_error(&error)?;
                continue;
            }
        };
        let selected_ids: Vec<_> = playlist
            .tracks
            .iter()
            .filter(|track| track.channel == channel)
            .map(|track| track.message_id)
            .collect();
        if let Some(selected) = choose_catalog_tracks(&catalog, &selected_ids)? {
            playlist.tracks.retain(|track| track.channel != channel);
            playlist.tracks.extend(selected);
            playlist.channel = playlist
                .tracks
                .first()
                .map(|track| track.channel.clone())
                .unwrap_or_default();
            playlist.local_tracks.clear();
        }
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

fn show_menu_error(error: &anyhow::Error) -> Result<()> {
    clear_screen()?;
    println!("{error:#}");
    prompt("Press Enter to go back...")?;
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
            (Some("track"), Some(channel), Some(message_id), Some(name)) => {
                if let Some(playlist) = playlists.last_mut()
                    && let Ok(message_id) = message_id.parse()
                {
                    playlist.tracks.push(TelegramCatalogEntry {
                        channel: normalize_channel(channel),
                        message_id,
                        name: name.to_string(),
                    });
                }
            }
            (Some("track"), Some(message_id), Some(name), None) => {
                if let Some(playlist) = playlists.last_mut()
                    && let Ok(message_id) = message_id.parse()
                {
                    playlist.tracks.push(TelegramCatalogEntry {
                        channel: playlist.channel.clone(),
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
                "track\t{}\t{}\t{}\n",
                normalize_channel(&track.channel),
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
    println!("  music-terminal-player [--borderless]");
    println!("  music-terminal-player [--borderless] play <file-or-folder>");
    println!("  music-terminal-player login");
    println!("  music-terminal-player stream <public-channel>");
    println!("  music-terminal-player sync <public-channel>");
    println!("  music-terminal-player download <public-channel> [folder]");
    println!("  music-terminal-player youtube");
    println!();
    println!("Controls while playing:");
    println!("  p/Space play/pause | n next | v previous | r shuffle | l loop mode");
    println!("  u queue | Up/Down and +/- volume 10% | Left/Right volume 1%");
    println!("  b menu | q/Ctrl+C quit");
    println!();
    println!("Telegram login and streaming need TG_ID and TG_HASH from https://my.telegram.org");
}
