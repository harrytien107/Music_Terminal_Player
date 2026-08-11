use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::runtime;

use crate::local::{
    choose_local_tracks, collect_tracks, play_path, play_tracks, play_tracks_shuffled,
};
use crate::telegram::{
    SESSION_FILE, TELEGRAM_CREDENTIALS_FILE, TelegramCatalogEntry, channel_catalog,
    choose_catalog_tracks, choose_private_channel, download_channel, normalize_channel_identity,
    play_catalog_entries, private_channel_from_link, stream_catalog_entries, sync_catalog,
    telegram_channel_label, telegram_client,
};
use crate::util::{
    LIBRARY_FILE, PLAYLIST_FILE, PlayerExit, RawMode, clear_screen, draw_frame, normalize_channel,
    prompt, select_menu, toggle_all,
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
        "▶  Play YouTube audio".to_string(),
        "♫  Play local folder".to_string(),
        "☁  Stream Telegram channel".to_string(),
        "↻  Sync and update Telegram channel".to_string(),
        "↓  Download Telegram channel to .\\music".to_string(),
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
            Some(1) => match crate::youtube::play_youtube() {
                Ok(PlayerExit::Quit) => return Ok(()),
                Ok(PlayerExit::Back) => {}
                Err(error) => show_menu_error(&error)?,
            },
            Some(2) => {
                while let Some(path) = choose_local_folder(&mut library)? {
                    match play_path(&path) {
                        Ok(PlayerExit::Quit) => return Ok(()),
                        Ok(PlayerExit::Back) => {}
                        Err(error) => show_menu_error(&error)?,
                    }
                }
            }
            Some(3) => {
                while let Some((label, catalog)) =
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
            Some(4) => {
                while let Some(channels) = choose_channel_to_sync(&mut library)? {
                    let total = channels.len();
                    let mut summary = Vec::with_capacity(total);
                    clear_screen()?;
                    for (index, (channel, save_after_sync)) in channels.into_iter().enumerate() {
                        let label = telegram_channel_label(&channel);
                        if index > 0 {
                            println!();
                        }
                        println!("Synchronizing channel {}/{}: {label}", index + 1, total);
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
                                summary.push(format!("[OK] {label}"));
                            }
                            Err(error) => summary.push(format!("[FAILED] {label}: {error:#}")),
                        }
                    }
                    println!("\nSynchronization batch complete ({total} channels):");
                    for result in summary {
                        println!("  {result}");
                    }
                    prompt("\nPress Enter to continue...")?;
                }
            }
            Some(5) => {
                if let Some(channel) =
                    choose_saved_channel(&library, "Download saved Telegram channel")?
                {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(download_channel(&channel, Path::new("music")))?;
                }
            }
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
                        .block_on(play_catalog_entries("", catalog, 0, true, 0))
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
            library.channels.push(normalize_channel_identity(channel));
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
    const PAGE_SIZE: usize = 20;
    if library.channels.is_empty() {
        clear_screen()?;
        println!("No synchronized Telegram channels.");
        prompt("Use 'Sync and update Telegram channel' to add one. Press Enter to return...")?;
        return Ok(None);
    }

    let raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut selected_row = 0usize;
    let mut selected = HashSet::new();
    loop {
        let start = selected_row
            .saturating_sub(PAGE_SIZE / 2)
            .min(library.channels.len().saturating_sub(PAGE_SIZE));
        let end = (start + PAGE_SIZE).min(library.channels.len());
        let mut frame = format!(
            "{title}\r\n{} selected | {} synchronized\r\n\r\n",
            selected.len(),
            library.channels.len()
        );
        for (offset, channel) in library.channels[start..end].iter().enumerate() {
            let channel_index = start + offset;
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if channel_index == selected_row {
                    ">"
                } else {
                    " "
                },
                if selected.contains(&channel_index) {
                    "x"
                } else {
                    " "
                },
                telegram_channel_label(channel)
            ));
        }
        frame.push_str(
            "\r\n[Space] toggle | [Ctrl+A] toggle all | Up/Down/Page Up/Page Down scroll | [Enter] play | [Esc] back",
        );
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected_row = selected_row
                    .checked_sub(1)
                    .unwrap_or(library.channels.len() - 1);
            }
            KeyCode::Down => selected_row = (selected_row + 1) % library.channels.len(),
            KeyCode::PageUp => selected_row = selected_row.saturating_sub(PAGE_SIZE),
            KeyCode::PageDown => {
                selected_row = (selected_row + PAGE_SIZE).min(library.channels.len() - 1);
            }
            KeyCode::Char(' ') => {
                if !selected.remove(&selected_row) {
                    selected.insert(selected_row);
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                toggle_all(&mut selected, 0..library.channels.len());
            }
            KeyCode::Enter => {
                let indexes = selected_or_highlighted_channel_indexes(&selected, selected_row);
                drop(raw);
                clear_screen()?;
                let mut tracks = Vec::new();
                for index in &indexes {
                    tracks.extend(channel_catalog(&library.channels[*index])?);
                }
                let label = if indexes.len() == 1 {
                    format!(
                        "Telegram channel: {}\n{} tracks",
                        telegram_channel_label(&library.channels[indexes[0]]),
                        tracks.len()
                    )
                } else {
                    format!(
                        "{} selected Telegram channels\n{} tracks",
                        indexes.len(),
                        tracks.len()
                    )
                };
                return Ok(Some((label, tracks)));
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn selected_or_highlighted_channel_indexes(
    selected: &HashSet<usize>,
    highlighted: usize,
) -> Vec<usize> {
    if selected.is_empty() {
        return vec![highlighted];
    }
    let mut indexes: Vec<_> = selected.iter().copied().collect();
    indexes.sort_unstable();
    indexes
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
        .map(|channel| telegram_channel_label(channel))
        .collect();
    let channel_count = items.len();
    items.push("←  Back".to_string());
    Ok(select_menu(title, &items)?
        .and_then(|index| (index < channel_count).then(|| library.channels[index].clone())))
}

fn choose_channel_to_sync(library: &mut SavedLibrary) -> Result<Option<Vec<(String, bool)>>> {
    loop {
        let channel_count = library.channels.len();
        let mut items = Vec::new();
        if channel_count > 0 {
            items.push("↻  Sync and update All channels".to_string());
            items.extend(
                library.channels.iter().map(|channel| {
                    format!("↻  Sync and update {}", telegram_channel_label(channel))
                }),
            );
            items.last_mut().unwrap().push_str(
                "\r\n\r\n╭────────────────────────────╮\r\n│       ADD OR MANAGE        │\r\n╰────────────────────────────╯",
            );
        }
        let saved_item_count = items.len();
        items.extend([
            "＋  Add public channel by username".to_string(),
            "＋  Add private channel from this account".to_string(),
            "＋  Add joined private channel by invite link".to_string(),
            "−  Forget channel".to_string(),
            "←  Back".to_string(),
        ]);
        let section = if channel_count == 0 {
            "╭────────────────────────────╮\n│       ADD OR MANAGE        │\n╰────────────────────────────╯"
        } else {
            "╭────────────────────────────╮\n│       SAVED CHANNELS       │\n╰────────────────────────────╯"
        };
        let title = format!(
            "Sync and update Telegram channel\nAdd an accessible channel or paste an invite link for one already joined.\n\n{section}"
        );
        match select_menu(&title, &items)? {
            Some(0) if channel_count > 0 => {
                return Ok(Some(
                    library
                        .channels
                        .iter()
                        .cloned()
                        .map(|channel| (channel, false))
                        .collect(),
                ));
            }
            Some(index) if channel_count > 0 && index < saved_item_count => {
                return Ok(Some(vec![(library.channels[index - 1].clone(), false)]));
            }
            Some(index) if index == saved_item_count => {
                clear_screen()?;
                let channel = normalize_channel(&prompt("Public channel: ")?);
                if !channel.is_empty() {
                    return Ok(Some(vec![(
                        channel.clone(),
                        !library.channels.contains(&channel),
                    )]));
                }
            }
            Some(index) if index == saved_item_count + 1 => {
                let channel = runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?
                    .block_on(choose_private_channel(&library.channels));
                match channel {
                    Ok(Some(channels)) => {
                        return Ok(Some(
                            channels
                                .into_iter()
                                .map(|channel| {
                                    let save_after_sync = !library.channels.contains(&channel);
                                    (channel, save_after_sync)
                                })
                                .collect(),
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => show_menu_error(&error)?,
                }
            }
            Some(index) if index == saved_item_count + 2 => {
                clear_screen()?;
                let link = prompt("Private channel invite link (or type 'back'): ")?;
                if link.trim().eq_ignore_ascii_case("back") || link.trim().is_empty() {
                    continue;
                }
                let channel = runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?
                    .block_on(private_channel_from_link(&link));
                match channel {
                    Ok(channel) => {
                        return Ok(Some(vec![(
                            channel.clone(),
                            !library.channels.contains(&channel),
                        )]));
                    }
                    Err(error) => show_menu_error(&error)?,
                }
            }
            Some(index) if index == saved_item_count + 3 => forget_channel(library)?,
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
    const PAGE_SIZE: usize = 20;
    if library.channels.is_empty() {
        clear_screen()?;
        prompt("No saved Telegram channels. Press Enter to return...")?;
        return Ok(());
    }

    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut selected_row = 0usize;
    let mut selected = HashSet::new();
    loop {
        let start = selected_row
            .saturating_sub(PAGE_SIZE / 2)
            .min(library.channels.len().saturating_sub(PAGE_SIZE));
        let end = (start + PAGE_SIZE).min(library.channels.len());
        let mut frame = format!(
            "Forget Telegram channels\r\n{} selected | {} saved\r\n\r\n",
            selected.len(),
            library.channels.len()
        );
        for (index, channel) in library.channels[start..end].iter().enumerate() {
            let channel_index = start + index;
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if channel_index == selected_row {
                    ">"
                } else {
                    " "
                },
                if selected.contains(&channel_index) {
                    "x"
                } else {
                    " "
                },
                telegram_channel_label(channel)
            ));
        }
        frame.push_str(
            "\r\n[Space] toggle | [Ctrl+A] toggle all | Up/Down/Page Up/Page Down scroll | [Enter] forget | [Esc] cancel",
        );
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected_row = selected_row
                    .checked_sub(1)
                    .unwrap_or(library.channels.len() - 1);
            }
            KeyCode::Down => selected_row = (selected_row + 1) % library.channels.len(),
            KeyCode::PageUp => selected_row = selected_row.saturating_sub(PAGE_SIZE),
            KeyCode::PageDown => {
                selected_row = (selected_row + PAGE_SIZE).min(library.channels.len() - 1);
            }
            KeyCode::Char(' ') => {
                if !selected.remove(&selected_row) {
                    selected.insert(selected_row);
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                toggle_all(&mut selected, 0..library.channels.len());
            }
            KeyCode::Enter => {
                if selected.is_empty() {
                    selected.insert(selected_row);
                }
                library.channels = library
                    .channels
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !selected.contains(index))
                    .map(|(_, channel)| channel.clone())
                    .collect();
                save_library(library)?;
                return Ok(());
            }
            KeyCode::Esc => return Ok(()),
            _ => {}
        }
    }
}

fn choose_local_folder(library: &mut SavedLibrary) -> Result<Option<PathBuf>> {
    loop {
        let mut items: Vec<_> = library
            .folders
            .iter()
            .map(|folder| format!("♫  {}", folder.display()))
            .collect();
        let folder_count = items.len();
        items.extend([
            "＋  Add folder".to_string(),
            "−  Forget location".to_string(),
            "←  Back".to_string(),
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
        .map(|folder| format!("♫  {}", folder.display()))
        .collect();
    let folder_count = items.len();
    items.push("←  Cancel".to_string());
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
                    "▶  {} ({source}, {} songs)",
                    playlist.name,
                    playlist.song_count()
                )
            })
            .collect();
        let playlist_count = items.len();
        items.extend([
            "＋  Create playlist".to_string(),
            "✎  Edit playlist songs".to_string(),
            "✎  Rename playlist".to_string(),
            "−  Delete playlist".to_string(),
            "←  Back".to_string(),
        ]);
        match select_menu("Playlists", &items)? {
            Some(index) if index < playlist_count => {
                let playlist = playlists[index].clone();
                let result = if playlist.is_local() {
                    play_tracks(playlist.local_tracks)
                } else {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(play_catalog_entries(
                            &playlist.channel,
                            playlist.tracks,
                            0,
                            false,
                            0,
                        ))
                };
                match result {
                    Ok(PlayerExit::Quit) => return Ok(PlayerExit::Quit),
                    Ok(PlayerExit::Back) => {}
                    Err(error) => show_menu_error(&error)?,
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
        &[
            "☁  Telegram channel".to_string(),
            "♫  Local folder".to_string(),
        ],
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
        &[
            "☁  Telegram channel".to_string(),
            "♫  Local folder".to_string(),
        ],
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
    items.push("←  Cancel".to_string());
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
                channel: normalize_channel_identity(channel),
                tracks: Vec::new(),
                local_tracks: Vec::new(),
            }),
            (Some("track"), Some(channel), Some(message_id), Some(name)) => {
                if let Some(playlist) = playlists.last_mut()
                    && let Ok(message_id) = message_id.parse()
                {
                    playlist.tracks.push(TelegramCatalogEntry {
                        channel: normalize_channel_identity(channel),
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
            normalize_channel_identity(&playlist.channel)
        ));
        for track in &playlist.tracks {
            text.push_str(&format!(
                "track\t{}\t{}\t{}\n",
                normalize_channel_identity(&track.channel),
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
