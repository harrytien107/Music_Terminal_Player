use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use grammers_client::media::Media;
use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use tokio::task::JoinHandle;

use crate::util::{
    LoopMode, PlayerExit, RawMode, SEEK_SECONDS, clear_screen, draw_frame, draw_panel,
    format_duration, format_elapsed, is_supported_audio_path, min_duration, normalize_channel,
    progress_bar, prompt, safe_file_name, select_menu, shuffle_slice,
};

pub(crate) const SESSION_FILE: &str = ".telegram.session";
pub(crate) const TELEGRAM_CREDENTIALS_FILE: &str = ".telegram.credentials";
const TELEGRAM_CACHE_DIR: &str = ".telegram-cache";
const INITIAL_BUFFER_BYTES: u64 = 1024 * 1024;
const TRACK_LIST_PAGE_SIZE: usize = 25;
#[derive(Clone)]
struct TelegramTrack {
    name: String,
    media: Media,
    size: Option<u64>,
    cache_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TelegramCatalogEntry {
    pub(crate) message_id: i32,
    pub(crate) name: String,
}

#[derive(Default)]
struct DownloadState {
    downloaded: u64,
    total: Option<u64>,
    complete: bool,
    buffering: bool,
    cancelled: bool,
    error: Option<String>,
}

struct SharedDownload {
    state: Mutex<DownloadState>,
    changed: Condvar,
}

impl SharedDownload {
    fn new(total: Option<u64>) -> Self {
        Self {
            state: Mutex::new(DownloadState {
                total,
                buffering: true,
                ..DownloadState::default()
            }),
            changed: Condvar::new(),
        }
    }

    fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.cancelled = true;
            self.changed.notify_all();
        }
    }
}

struct ProgressiveReader {
    file: File,
    position: u64,
    shared: Arc<SharedDownload>,
}

impl Read for ProgressiveReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let available = {
            let mut state = self.shared.state.lock().map_err(lock_error)?;
            while self.position >= state.downloaded
                && !state.complete
                && !state.cancelled
                && state.error.is_none()
            {
                state.buffering = true;
                state = self.shared.changed.wait(state).map_err(lock_error)?;
            }
            state.buffering = false;

            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if state.cancelled || (state.complete && self.position >= state.downloaded) {
                return Ok(0);
            }
            (state.downloaded - self.position).min(buffer.len() as u64) as usize
        };

        self.file.seek(SeekFrom::Start(self.position))?;
        let read = self.file.read(&mut buffer[..available])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let total = self.shared.state.lock().map_err(lock_error)?.total;
        let position = match from {
            SeekFrom::Start(position) => position,
            SeekFrom::Current(offset) => checked_position(self.position, offset)?,
            SeekFrom::End(offset) => checked_position(
                total.ok_or_else(|| io::Error::other("Telegram file size is unknown"))?,
                offset,
            )?,
        };
        self.position = position;
        Ok(position)
    }
}

pub(crate) fn checked_position(base: u64, offset: i64) -> io::Result<u64> {
    let position = base as i128 + offset as i128;
    if !(0..=u64::MAX as i128).contains(&position) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"));
    }
    Ok(position as u64)
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("progressive download state lock poisoned")
}

pub(crate) async fn telegram_client() -> Result<Client> {
    let (api_id, api_hash) = telegram_credentials()?;
    let session = Arc::new(SqliteSession::open(SESSION_FILE).await?);
    let SenderPool { runner, handle, .. } = SenderPool::new(session, api_id);
    let client = Client::new(handle);
    tokio::spawn(runner.run());

    if !client.is_authorized().await? {
        println!("Signing in to Telegram...");
        let phone = prompt("Phone number, international format: ")?;
        let token = client.request_login_code(phone.trim(), &api_hash).await?;
        let code = prompt("Login code: ")?;
        match client.sign_in(&token, code.trim()).await {
            Err(SignInError::PasswordRequired(password_token)) => {
                let hint = password_token.hint().unwrap_or("");
                let password = rpassword::prompt_password(format!("Two-step password ({hint}): "))?;
                client
                    .check_password(password_token, password.trim())
                    .await?;
            }
            Ok(_) => {}
            Err(err) => return Err(err.into()),
        }
        println!("Signed in. Session saved in {SESSION_FILE}.");
    }

    Ok(client)
}

fn telegram_credentials() -> Result<(i32, String)> {
    let env_id = env::var("TG_ID").ok();
    let env_hash = env::var("TG_HASH").ok();
    if let (Some(id), Some(hash)) = (env_id, env_hash) {
        return Ok((id.parse().context("TG_ID must be an integer")?, hash));
    }

    if let Ok(text) = fs::read_to_string(TELEGRAM_CREDENTIALS_FILE) {
        let mut lines = text.lines();
        let id = lines
            .next()
            .context("saved Telegram credentials are missing api_id")?
            .trim()
            .parse()
            .context("saved Telegram api_id must be an integer")?;
        let hash = lines
            .next()
            .context("saved Telegram credentials are missing api_hash")?
            .trim()
            .to_string();
        if !hash.is_empty() {
            return Ok((id, hash));
        }
    }

    println!("Telegram setup: create api_id and api_hash at https://my.telegram.org");
    let id = prompt("Telegram api_id: ")?
        .trim()
        .parse()
        .context("Telegram api_id must be an integer")?;
    let hash = prompt("Telegram api_hash: ")?.trim().to_string();
    if hash.is_empty() {
        bail!("Telegram api_hash cannot be empty");
    }
    fs::write(TELEGRAM_CREDENTIALS_FILE, format!("{id}\n{hash}\n"))?;
    Ok((id, hash))
}

pub(crate) async fn stream_channel(channel: &str) -> Result<PlayerExit> {
    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
    let username = normalize_channel(channel);
    let catalog_path = telegram_catalog_path(&username);
    let catalog = load_telegram_catalog(&catalog_path).with_context(|| {
        format!("no usable song list for @{username}; run Sync Telegram channel first")
    })?;
    if catalog.is_empty() {
        bail!("song list for @{username} is empty; run synchronization again");
    }

    let menu = vec![
        "Play in order".to_string(),
        "Shuffle".to_string(),
        "Search and choose a track".to_string(),
        "Back".to_string(),
    ];
    let menu_title = format!(
        "Telegram channel: @{username}\n{} tracks | {}",
        catalog.len(),
        catalog_path.display()
    );
    let (catalog_index, shuffle) = loop {
        match select_menu(&menu_title, &menu)? {
            Some(0) => break (0, false),
            Some(1) => break (0, true),
            Some(2) => {
                if let Some(index) = choose_catalog_track(&catalog, &catalog_path)? {
                    break (index, false);
                }
            }
            Some(3) | None => return Ok(PlayerExit::Back),
            _ => unreachable!(),
        }
    };
    play_catalog_entries(&username, catalog, catalog_index, shuffle).await
}

pub(crate) fn channel_catalog(channel: &str) -> Result<Vec<TelegramCatalogEntry>> {
    let username = normalize_channel(channel);
    let path = telegram_catalog_path(&username);
    load_telegram_catalog(&path).with_context(|| {
        format!("no song list for @{username}; update the Telegram song list first")
    })
}

pub(crate) async fn play_catalog_entries(
    channel: &str,
    catalog: Vec<TelegramCatalogEntry>,
    catalog_index: usize,
    shuffle: bool,
) -> Result<PlayerExit> {
    if catalog.is_empty() {
        bail!("playlist has no songs");
    }
    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
    let username = normalize_channel(channel);
    clear_screen()?;
    println!("Loading selected Telegram track from @{username}...");
    let client = telegram_client().await?;
    let cache_dir = Path::new(TELEGRAM_CACHE_DIR).join(safe_file_name(&username));
    fs::create_dir_all(&cache_dir)?;
    let catalog_index = catalog_index.min(catalog.len() - 1);
    play_telegram_tracks(client, username, cache_dir, catalog, catalog_index, shuffle).await
}

fn choose_catalog_track(
    catalog: &[TelegramCatalogEntry],
    catalog_path: &Path,
) -> Result<Option<usize>> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut query = String::new();
    let mut matches: Vec<_> = (0..catalog.len()).collect();
    let mut selected = 0usize;

    loop {
        if !matches.is_empty() {
            selected = selected.min(matches.len() - 1);
        } else {
            selected = 0;
        }
        let start = selected
            .saturating_sub(TRACK_LIST_PAGE_SIZE / 2)
            .min(matches.len().saturating_sub(TRACK_LIST_PAGE_SIZE));
        let end = (start + TRACK_LIST_PAGE_SIZE).min(matches.len());
        let mut frame = format!(
            "Search songs\r\nList: {}\r\nSearch: {query}_\r\n{} match(es)\r\n\r\n",
            catalog_path.display(),
            matches.len()
        );
        for (match_index, &catalog_index) in matches[start..end].iter().enumerate() {
            let absolute_index = start + match_index;
            frame.push_str(&format!(
                "{} {}\r\n",
                if absolute_index == selected { ">" } else { " " },
                catalog[catalog_index].name
            ));
        }
        if matches.is_empty() {
            frame.push_str("No matching tracks.\r\n");
        }
        frame.push_str(
            "\r\nType to search | Backspace edit | Up/Down select | Enter play | Esc back",
        );
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up if !matches.is_empty() => {
                selected = selected.checked_sub(1).unwrap_or(matches.len() - 1);
            }
            KeyCode::Down if !matches.is_empty() => selected = (selected + 1) % matches.len(),
            KeyCode::Enter if !matches.is_empty() => return Ok(Some(matches[selected])),
            KeyCode::Backspace => {
                query.pop();
                matches = search_catalog(catalog, &query);
                selected = 0;
            }
            KeyCode::Char(character) if !character.is_control() => {
                query.push(character);
                matches = search_catalog(catalog, &query);
                selected = 0;
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn choose_catalog_tracks(
    catalog: &[TelegramCatalogEntry],
    selected_ids: &[i32],
) -> Result<Option<Vec<TelegramCatalogEntry>>> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut query = String::new();
    let mut matches: Vec<_> = (0..catalog.len()).collect();
    let mut selected_row = 0usize;
    let mut selected: HashSet<i32> = selected_ids.iter().copied().collect();

    loop {
        if !matches.is_empty() {
            selected_row = selected_row.min(matches.len() - 1);
        } else {
            selected_row = 0;
        }
        let start = selected_row
            .saturating_sub(TRACK_LIST_PAGE_SIZE / 2)
            .min(matches.len().saturating_sub(TRACK_LIST_PAGE_SIZE));
        let end = (start + TRACK_LIST_PAGE_SIZE).min(matches.len());
        let mut frame = format!(
            "Choose playlist songs\r\nSearch: {query}_ | {} selected | {} matches\r\n\r\n",
            selected.len(),
            matches.len()
        );
        for (offset, &catalog_index) in matches[start..end].iter().enumerate() {
            let entry = &catalog[catalog_index];
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if start + offset == selected_row {
                    ">"
                } else {
                    " "
                },
                if selected.contains(&entry.message_id) {
                    "x"
                } else {
                    " "
                },
                entry.name
            ));
        }
        if matches.is_empty() {
            frame.push_str("No matching tracks.\r\n");
        }
        frame.push_str(
            "\r\nType to search | [Space] toggle | Up/Down select | [Enter] save | [Esc] cancel",
        );
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up if !matches.is_empty() => {
                selected_row = selected_row.checked_sub(1).unwrap_or(matches.len() - 1);
            }
            KeyCode::Down if !matches.is_empty() => {
                selected_row = (selected_row + 1) % matches.len();
            }
            KeyCode::Char(' ') if !matches.is_empty() => {
                let id = catalog[matches[selected_row]].message_id;
                if !selected.remove(&id) {
                    selected.insert(id);
                }
            }
            KeyCode::Backspace => {
                query.pop();
                matches = search_catalog(catalog, &query);
                selected_row = 0;
            }
            KeyCode::Char(character) if !character.is_control() => {
                query.push(character);
                matches = search_catalog(catalog, &query);
                selected_row = 0;
            }
            KeyCode::Enter => {
                return Ok(Some(
                    catalog
                        .iter()
                        .filter(|entry| selected.contains(&entry.message_id))
                        .cloned()
                        .collect(),
                ));
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn search_catalog(catalog: &[TelegramCatalogEntry], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    catalog
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.name.to_lowercase().contains(&query).then_some(index))
        .collect()
}

struct ActiveTelegramTrack {
    sink: Sink,
    duration: Option<Duration>,
    cache_path: PathBuf,
    shared: Arc<SharedDownload>,
    download_task: Option<JoinHandle<()>>,
}

impl ActiveTelegramTrack {
    async fn stop(mut self) {
        self.shared.cancel();
        self.sink.stop();
        if let Some(task) = self.download_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn play_telegram_tracks(
    client: Client,
    username: String,
    cache_dir: PathBuf,
    mut tracks: Vec<TelegramCatalogEntry>,
    mut index: usize,
    mut shuffle: bool,
) -> Result<PlayerExit> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let original_tracks = tracks.clone();
    if shuffle {
        shuffle_slice(&mut tracks);
        index = 0;
    }
    let stream = OutputStreamBuilder::open_default_stream()
        .context("failed to open default audio output")?;
    let mut volume = 0.8f32;
    let mut loop_mode = LoopMode::Off;
    let mut active = start_catalog_track(
        &client,
        &username,
        &cache_dir,
        &stream,
        &tracks[index],
        volume,
    )
    .await?;

    loop {
        draw_telegram_player(
            &mut stdout,
            &tracks,
            index,
            active.duration,
            &active.sink,
            volume,
            shuffle,
            &active.shared,
            loop_mode,
        )?;

        let complete = active.shared.state.lock().map_err(lock_error)?.complete;
        if complete && active.sink.empty() {
            let old_cache = active.cache_path.clone();
            active.stop().await;
            delete_cache_file(&old_cache)?;
            match loop_mode {
                LoopMode::One => {}
                LoopMode::All => index = (index + 1) % tracks.len(),
                LoopMode::Off if index + 1 < tracks.len() => index += 1,
                LoopMode::Off => {
                    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                    return Ok(PlayerExit::Back);
                }
            }
            active = start_catalog_track(
                &client,
                &username,
                &cache_dir,
                &stream,
                &tracks[index],
                volume,
            )
            .await?;
            continue;
        }
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                active.stop().await;
                delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                return Ok(PlayerExit::Quit);
            }
            KeyCode::Char('b') => {
                active.stop().await;
                delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                return Ok(PlayerExit::Back);
            }
            KeyCode::Char('p') => {
                if active.sink.is_paused() {
                    active.sink.play();
                } else {
                    active.sink.pause();
                }
            }
            KeyCode::Char('n') => {
                let old_cache = active.cache_path.clone();
                active.stop().await;
                delete_cache_file(&old_cache)?;
                index = (index + 1) % tracks.len();
                active = start_catalog_track(
                    &client,
                    &username,
                    &cache_dir,
                    &stream,
                    &tracks[index],
                    volume,
                )
                .await?;
            }
            KeyCode::Char('v') => {
                let old_cache = active.cache_path.clone();
                active.stop().await;
                delete_cache_file(&old_cache)?;
                index = if index == 0 {
                    tracks.len() - 1
                } else {
                    index - 1
                };
                active = start_catalog_track(
                    &client,
                    &username,
                    &cache_dir,
                    &stream,
                    &tracks[index],
                    volume,
                )
                .await?;
            }
            KeyCode::Left => {
                let target = active
                    .sink
                    .get_pos()
                    .saturating_sub(Duration::from_secs(SEEK_SECONDS));
                let _ = active.sink.try_seek(target);
            }
            KeyCode::Right => {
                let mut target = active.sink.get_pos() + Duration::from_secs(SEEK_SECONDS);
                if let Some(total) = active.duration {
                    target = min_duration(target, total);
                }
                let _ = active.sink.try_seek(target);
            }
            KeyCode::Char('l') => loop_mode = loop_mode.cycle(),
            KeyCode::Char('r') => {
                let current_message_id = tracks[index].message_id;
                shuffle = !shuffle;
                tracks = if shuffle {
                    let mut shuffled = original_tracks.clone();
                    shuffle_slice(&mut shuffled);
                    shuffled
                } else {
                    original_tracks.clone()
                };
                index = tracks
                    .iter()
                    .position(|track| track.message_id == current_message_id)
                    .unwrap_or(0);
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.05).min(1.5);
                active.sink.set_volume(volume);
            }
            KeyCode::Char('-') => {
                volume = (volume - 0.05).max(0.0);
                active.sink.set_volume(volume);
            }
            _ => {}
        }
    }
}

fn delete_cache_file(path: &Path) -> Result<()> {
    for attempt in 0..5 {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt < 4 => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to delete cache file {}", path.display()));
            }
        }
    }
    unreachable!()
}

pub(crate) fn delete_cache_directory(path: &Path) -> Result<()> {
    for attempt in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt < 4 => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to delete cache {}", path.display()));
            }
        }
    }
    unreachable!()
}

async fn start_catalog_track(
    client: &Client,
    username: &str,
    cache_dir: &Path,
    stream: &OutputStream,
    entry: &TelegramCatalogEntry,
    volume: f32,
) -> Result<ActiveTelegramTrack> {
    let peer = client
        .resolve_username(username)
        .await?
        .with_context(|| format!("public channel not found: {username}"))?
        .to_ref()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .with_context(|| format!("cannot access channel: {username}"))?;
    let message = client
        .get_messages_by_id(peer, &[entry.message_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .with_context(|| {
            format!(
                "Telegram track '{}' is no longer available; update the song list",
                entry.name
            )
        })?;
    let media = message.media().with_context(|| {
        format!(
            "Telegram track '{}' no longer contains media; update the song list",
            entry.name
        )
    })?;
    if !is_audio_media(&media) {
        bail!(
            "Telegram track '{}' is no longer audio; update the song list",
            entry.name
        );
    }
    let track = TelegramTrack {
        name: entry.name.clone(),
        size: media.size().map(|size| size as u64),
        cache_path: cache_dir.join(format!(
            "{}-{}",
            entry.message_id,
            safe_file_name(&entry.name)
        )),
        media,
    };
    start_telegram_track(client, stream, &track, volume).await
}

async fn start_telegram_track(
    client: &Client,
    stream: &OutputStream,
    track: &TelegramTrack,
    volume: f32,
) -> Result<ActiveTelegramTrack> {
    let shared = Arc::new(SharedDownload::new(track.size));
    let mut download_task = None;
    let cached_size = fs::metadata(&track.cache_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let cache_complete = track
        .size
        .is_some_and(|size| cached_size == size && size > 0);

    if cache_complete {
        let mut state = shared.state.lock().map_err(lock_error)?;
        state.downloaded = cached_size;
        state.complete = true;
        state.buffering = false;
    } else {
        if track.cache_path.exists() {
            fs::remove_file(&track.cache_path)?;
        }
        File::create(&track.cache_path)?;
        let client = client.clone();
        let media = track.media.clone();
        let path = track.cache_path.clone();
        let download = Arc::clone(&shared);
        download_task = Some(tokio::spawn(async move {
            if let Err(error) =
                download_progressively(client, media, path, Arc::clone(&download)).await
            {
                if let Ok(mut state) = download.state.lock() {
                    state.error = Some(error.to_string());
                    state.buffering = false;
                    download.changed.notify_all();
                }
            }
        }));

        loop {
            let ready = {
                let state = shared.state.lock().map_err(lock_error)?;
                if let Some(error) = &state.error {
                    bail!("Telegram download failed: {error}");
                }
                state.downloaded
                    >= INITIAL_BUFFER_BYTES.min(state.total.unwrap_or(INITIAL_BUFFER_BYTES))
                    || state.complete
            };
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let reader = ProgressiveReader {
        file: open_cache_for_read(&track.cache_path)?,
        position: 0,
        shared: Arc::clone(&shared),
    };
    let decoder = Decoder::try_from(BufReader::new(reader))
        .with_context(|| format!("failed to decode Telegram track {}", track.name))?;
    let duration = decoder.total_duration();
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    sink.append(decoder);
    Ok(ActiveTelegramTrack {
        sink,
        duration,
        cache_path: track.cache_path.clone(),
        shared,
        download_task,
    })
}

async fn download_progressively(
    client: Client,
    media: Media,
    path: PathBuf,
    shared: Arc<SharedDownload>,
) -> Result<()> {
    let mut output = open_cache_for_write(&path)?;
    let mut download = client.iter_download(&media).chunk_size(512 * 1024);
    while let Some(bytes) = download.next().await? {
        if shared.state.lock().map_err(lock_error)?.cancelled {
            return Ok(());
        }
        output.write_all(&bytes)?;
        output.flush()?;
        let mut state = shared.state.lock().map_err(lock_error)?;
        state.downloaded += bytes.len() as u64;
        shared.changed.notify_all();
    }
    output.flush()?;
    let mut state = shared.state.lock().map_err(lock_error)?;
    state.complete = true;
    state.buffering = false;
    if state.total.is_none() {
        state.total = Some(state.downloaded);
    }
    shared.changed.notify_all();
    Ok(())
}

fn open_cache_for_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.share_mode(0x1 | 0x2 | 0x4);
    options.open(path)
}

fn open_cache_for_write(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(windows)]
    options
        .access_mode(0x40000000 | 0x00010000)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x04000000);
    options.open(path)
}

fn draw_telegram_player(
    stdout: &mut io::Stdout,
    tracks: &[TelegramCatalogEntry],
    index: usize,
    duration: Option<Duration>,
    sink: &Sink,
    volume: f32,
    shuffle: bool,
    shared: &SharedDownload,
    loop_mode: LoopMode,
) -> Result<()> {
    let state = shared.state.lock().map_err(lock_error)?;
    let playback = if sink.is_paused() {
        "paused"
    } else if state.buffering {
        "buffering"
    } else {
        "playing"
    };
    let progress = state
        .total
        .filter(|total| *total > 0)
        .map(|total| format!("{:.0}%", state.downloaded as f64 * 100.0 / total as f64))
        .unwrap_or_else(|| format!("{} KiB", state.downloaded / 1024));
    let elapsed = sink.get_pos();
    let total = duration
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let rows = vec![
        format!(
            "Track {}/{} | {}",
            index + 1,
            tracks.len(),
            tracks[index].name
        ),
        String::new(),
        format!(
            "[{}] {}/{}",
            progress_bar(elapsed, duration, 12),
            format_duration(elapsed),
            total
        ),
        format!("{} | volume {:.0}%", playback, volume * 100.0),
        format!(
            "cache {progress} | shuffle {} | loop {}",
            if shuffle { "on" } else { "off" },
            loop_mode.label()
        ),
        String::new(),
        "[p] play/pause | [v] previous | [n] next | [r] shuffle | [l] loop".to_string(),
        "[Left/Right] seek | [+/-] volume | [b] menu | [q] quit".to_string(),
    ];
    draw_panel(stdout, "Music Terminal Player · Telegram", &rows)
}

pub(crate) async fn sync_catalog(channel: &str) -> Result<()> {
    scan_channel(channel, None).await
}

pub(crate) async fn download_channel(channel: &str, folder: &Path) -> Result<()> {
    fs::create_dir_all(folder).with_context(|| format!("failed to create {}", folder.display()))?;
    scan_channel(channel, Some(folder)).await
}

async fn scan_channel(channel: &str, download_folder: Option<&Path>) -> Result<()> {
    let started = Instant::now();
    let client = telegram_client().await?;
    let username = channel.trim_start_matches('@');
    let peer = client
        .resolve_username(username)
        .await?
        .with_context(|| format!("public channel not found: {username}"))?
        .to_ref()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .with_context(|| format!("cannot access channel: {username}"))?;

    let mut catalog = Vec::new();
    let mut pending = Vec::new();
    let mut messages = client.iter_messages(peer);

    println!("Scanning all songs in @{username}...");
    while let Some(message) = messages.next().await? {
        let Some(media) = message.media() else {
            continue;
        };
        if !is_audio_media(&media) {
            continue;
        }

        let name =
            media_file_name(&media).unwrap_or_else(|| format!("telegram-{}.bin", message.id()));
        catalog.push(TelegramCatalogEntry {
            message_id: message.id(),
            name: name.clone(),
        });
        if let Some(folder) = download_folder {
            let safe_name = safe_file_name(&name);
            let dest = folder.join(format!("{}-{}", message.id(), safe_name));
            if !dest.exists() {
                pending.push((media, dest));
            }
        }
    }

    catalog.reverse();
    let total_songs = catalog.len();
    let catalog_path = telegram_catalog_path(username);
    save_telegram_catalog(&catalog_path, &catalog)?;
    let scan_time = started.elapsed();
    println!(
        "Scan complete. Total songs: {total_songs}. Scan time: {}. Song list: {}",
        format_elapsed(scan_time),
        catalog_path.display()
    );
    let Some(folder) = download_folder else {
        println!("Song-list update complete.");
        return Ok(());
    };

    let download_total = pending.len();
    for (index, (media, dest)) in pending.into_iter().enumerate() {
        println!(
            "[{}/{}] Downloading {} | download time {}",
            index + 1,
            download_total,
            dest.display(),
            format_elapsed(started.elapsed().saturating_sub(scan_time))
        );
        client.download_media(&media, &dest).await?;
    }

    println!(
        "Download complete. Channel songs: {total_songs}, downloaded: {download_total}, download time: {}. Folder: {}. Song list: {}",
        format_elapsed(started.elapsed().saturating_sub(scan_time)),
        folder.display(),
        catalog_path.display()
    );
    Ok(())
}

fn is_audio_media(media: &Media) -> bool {
    match media {
        Media::Document(document) => {
            let mime_audio = document
                .mime_type()
                .map(|mime| mime.starts_with("audio/"))
                .unwrap_or(false);
            let ext_audio = document
                .name()
                .map(|name| is_supported_audio_path(Path::new(name)))
                .unwrap_or(false);
            mime_audio || ext_audio
        }
        _ => false,
    }
}

fn media_file_name(media: &Media) -> Option<String> {
    match media {
        Media::Document(document) => document.name().map(ToOwned::to_owned),
        _ => None,
    }
}

fn telegram_catalog_path(username: &str) -> PathBuf {
    PathBuf::from(format!(
        ".telegram-{}.tracks.txt",
        safe_file_name(&normalize_channel(username))
    ))
}

pub(crate) fn save_telegram_catalog(path: &Path, catalog: &[TelegramCatalogEntry]) -> Result<()> {
    let mut text = String::new();
    for entry in catalog {
        let name = entry.name.replace(['\t', '\r', '\n'], " ");
        text.push_str(&format!("{}\t{name}\n", entry.message_id));
    }
    fs::write(path, text)
        .with_context(|| format!("failed to save Telegram song list {}", path.display()))
}

pub(crate) fn load_telegram_catalog(path: &Path) -> Result<Vec<TelegramCatalogEntry>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read Telegram song list {}", path.display()))?;
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            let (message_id, name) = line.split_once('\t').with_context(|| {
                format!(
                    "invalid song list entry at {}:{}",
                    path.display(),
                    index + 1
                )
            })?;
            Ok(TelegramCatalogEntry {
                message_id: message_id.parse().with_context(|| {
                    format!("invalid message ID at {}:{}", path.display(), index + 1)
                })?,
                name: name.to_string(),
            })
        })
        .collect()
}
