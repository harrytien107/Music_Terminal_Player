use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use grammers_client::media::Media;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_client::{Client, SignInError};
use grammers_mtsender::{InvocationError, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecRegistry, Decoder as SymphoniaDecoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units;
use symphonia_adapter_libopus::OpusDecoder;
use tokio::task::JoinHandle;

use crate::util::{
    DATA_DIR, LoopMode, PlayerExit, RawMode, clear_screen, draw_frame, draw_panel, format_duration,
    format_elapsed, is_supported_audio_path, load_volume, manage_queue, normalize_channel,
    playback_controls, progress_bar, prompt, safe_file_name, save_volume, select_menu,
    shuffle_slice, toggle_all,
};

pub(crate) const SESSION_FILE: &str = ".music-terminal/telegram.session";
pub(crate) const TELEGRAM_CREDENTIALS_FILE: &str = ".music-terminal/telegram.credentials";
const TELEGRAM_CACHE_DIR: &str = ".music-terminal/telegram-cache";
const DOWNLOAD_CHUNK_BYTES: u64 = 512 * 1024;
const INITIAL_BUFFER_BYTES: u64 = 1024 * 1024;
const TRACK_LIST_PAGE_SIZE: usize = 25;
const PRIVATE_CHANNEL_PAGE_SIZE: usize = 20;
const PRIVATE_CHANNEL_PREFIX: &str = "private:";

#[derive(Clone)]
struct TelegramTrack {
    name: String,
    media: Media,
    size: Option<u64>,
    cache_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TelegramCatalogEntry {
    pub(crate) channel: String,
    pub(crate) message_id: i32,
    pub(crate) name: String,
}

#[derive(Default)]
struct DownloadState {
    downloaded: u64,
    total: Option<u64>,
    complete: bool,
    buffering: bool,
    reconnecting: bool,
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

impl MediaSource for ProgressiveReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.shared.state.lock().ok()?.total
    }
}

struct OpusSource {
    decoder: Box<dyn SymphoniaDecoder>,
    format: Box<dyn FormatReader>,
    track_id: u32,
    buffer: SampleBuffer<f32>,
    offset: usize,
    spec: SignalSpec,
    duration: Option<Duration>,
}

impl OpusSource {
    fn new(reader: ProgressiveReader) -> Result<Self> {
        let stream = MediaSourceStream::new(Box::new(reader), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("opus");
        let mut probed = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..FormatOptions::default()
                },
                &MetadataOptions::default(),
            )
            .context("failed to read Opus container")?;
        let track = probed
            .format
            .default_track()
            .context("Opus file contains no audio track")?;
        let track_id = track.id;
        let duration = track
            .codec_params
            .time_base
            .zip(track.codec_params.n_frames)
            .map(|(base, frames)| Duration::from(base.calc_time(frames)));
        let mut codecs = CodecRegistry::new();
        codecs.register_all::<OpusDecoder>();
        let mut decoder = codecs
            .make(&track.codec_params, &DecoderOptions::default())
            .context("failed to initialize Opus decoder")?;
        let (buffer, spec) = next_decoded_packet(&mut *probed.format, &mut *decoder, track_id)
            .context("Opus file contains no decodable audio")?;
        Ok(Self {
            decoder,
            format: probed.format,
            track_id,
            buffer,
            offset: 0,
            spec,
            duration,
        })
    }
}

fn next_decoded_packet(
    format: &mut dyn FormatReader,
    decoder: &mut dyn SymphoniaDecoder,
    track_id: u32,
) -> Option<(SampleBuffer<f32>, SignalSpec)> {
    loop {
        let packet = format.next_packet().ok()?;
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                return Some((sample_buffer(decoded, &spec), spec));
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => return None,
        }
    }
}

fn sample_buffer(decoded: AudioBufferRef<'_>, spec: &SignalSpec) -> SampleBuffer<f32> {
    let mut buffer = SampleBuffer::new(units::Duration::from(decoded.capacity() as u64), *spec);
    buffer.copy_interleaved_ref(decoded);
    buffer
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buffer.len() {
            let (buffer, spec) =
                next_decoded_packet(&mut *self.format, &mut *self.decoder, self.track_id)?;
            self.spec = spec;
            self.buffer = buffer;
            self.offset = 0;
        }
        let sample = *self.buffer.samples().get(self.offset)?;
        self.offset += 1;
        Some(sample)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len().saturating_sub(self.offset))
    }

    fn channels(&self) -> u16 {
        self.spec.channels.count() as u16
    }

    fn sample_rate(&self) -> u32 {
        self.spec.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
}

pub(crate) fn is_opus_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("opus"))
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

    if client.is_authorized().await? {
        return Ok(client);
    }

    clear_screen()?;
    println!("Signing in to Telegram...");
    'login: loop {
        let phone = loop {
            let phone = prompt("Phone number, international format (example +84901234567): ")?;
            let phone = phone.trim();
            if phone.len() > 1
                && phone.starts_with('+')
                && phone[1..].chars().all(|ch| ch.is_ascii_digit())
            {
                break phone.to_string();
            }
            println!("Invalid phone number. Include + and the country code.");
        };
        let token = loop {
            match client.request_login_code(&phone, &api_hash).await {
                Ok(token) => break token,
                Err(error) => {
                    println!("Could not request a login code: {error}");
                    let action =
                        prompt("Press Enter to retry, type 'phone', or type 'credentials': ")?;
                    if action.trim().eq_ignore_ascii_case("credentials") {
                        let _ = fs::remove_file(TELEGRAM_CREDENTIALS_FILE);
                        bail!("Telegram credentials cleared; retry login to enter them again");
                    }
                    if action.trim().eq_ignore_ascii_case("phone") {
                        continue 'login;
                    }
                }
            }
        };

        loop {
            let code = prompt("Login code: ")?;
            if code.trim().is_empty() {
                println!("Login code cannot be empty.");
                continue;
            }
            match client.sign_in(&token, code.trim()).await {
                Ok(_) => break 'login,
                Err(SignInError::InvalidCode) => println!("Invalid login code. Try again."),
                Err(SignInError::PasswordRequired(mut password_token)) => loop {
                    let hint = password_token.hint().unwrap_or("");
                    let password =
                        rpassword::prompt_password(format!("Two-step password ({hint}): "))?;
                    match client.check_password(password_token, password.trim()).await {
                        Ok(_) => break 'login,
                        Err(SignInError::InvalidPassword(next_token)) => {
                            println!("Invalid two-step password. Try again.");
                            password_token = next_token;
                        }
                        Err(error) => {
                            println!("Telegram login failed: {error}. Requesting a new code.");
                            continue 'login;
                        }
                    }
                },
                Err(error) => {
                    println!("Telegram login failed: {error}. Requesting a new code.");
                    continue 'login;
                }
            }
        }
    }
    println!("Signed in. Session saved in {SESSION_FILE}.");
    Ok(client)
}

fn valid_api_credentials(id: i32, hash: &str) -> bool {
    id > 0 && hash.len() == 32 && hash.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn telegram_credentials() -> Result<(i32, String)> {
    let env_id = env::var("TG_ID").ok();
    let env_hash = env::var("TG_HASH").ok();
    if let (Some(id), Some(hash)) = (env_id, env_hash) {
        let id = id.parse().context("TG_ID must be an integer")?;
        if valid_api_credentials(id, &hash) {
            return Ok((id, hash));
        }
        bail!("TG_ID/TG_HASH must contain a positive ID and a 32-character hexadecimal hash");
    }

    if let Ok(text) = fs::read_to_string(TELEGRAM_CREDENTIALS_FILE) {
        let mut lines = text.lines();
        if let (Some(id), Some(hash)) = (lines.next(), lines.next())
            && let Ok(id) = id.trim().parse()
        {
            let hash = hash.trim().to_string();
            if valid_api_credentials(id, &hash) {
                return Ok((id, hash));
            }
        }
        println!("Saved Telegram credentials are invalid. Enter them again.");
    }

    clear_screen()?;
    println!("Telegram setup: create api_id and api_hash at https://my.telegram.org");
    loop {
        let id = match prompt("Telegram api_id: ")?.trim().parse::<i32>() {
            Ok(id) if id > 0 => id,
            _ => {
                println!("Telegram api_id must be a positive integer.");
                continue;
            }
        };
        let hash = prompt("Telegram api_hash: ")?.trim().to_string();
        if !valid_api_credentials(id, &hash) {
            println!("Telegram api_hash must be 32 hexadecimal characters.");
            continue;
        }
        fs::write(TELEGRAM_CREDENTIALS_FILE, format!("{id}\n{hash}\n"))?;
        return Ok((id, hash));
    }
}

pub(crate) async fn stream_channel(channel: &str) -> Result<PlayerExit> {
    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
    let channel = normalize_channel_identity(channel);
    let label = telegram_channel_label(&channel);
    let catalog_path = telegram_catalog_path(&channel);
    let mut catalog = load_telegram_catalog(&catalog_path).with_context(|| {
        format!("no usable song list for {label}; run Sync and update Telegram channel first")
    })?;
    for entry in &mut catalog {
        entry.channel = channel.clone();
    }
    if catalog.is_empty() {
        bail!("song list for {label} is empty; run synchronization again");
    }

    let title = format!(
        "Telegram channel: {label}\n{} tracks | {}",
        catalog.len(),
        catalog_path.display()
    );
    stream_catalog_entries(&title, catalog).await
}

pub(crate) async fn stream_catalog_entries(
    title: &str,
    catalog: Vec<TelegramCatalogEntry>,
) -> Result<PlayerExit> {
    if catalog.is_empty() {
        bail!("selected Telegram channels contain no tracks");
    }
    let menu = vec![
        "Play in order".to_string(),
        "Shuffle".to_string(),
        "Search and choose a track".to_string(),
        "Back".to_string(),
    ];
    let (catalog_index, shuffle) = loop {
        match select_menu(title, &menu)? {
            Some(0) => break (0, false),
            Some(1) => break (0, true),
            Some(2) => {
                if let Some(index) = choose_catalog_track(&catalog, title)? {
                    break (index, false);
                }
            }
            Some(3) | None => return Ok(PlayerExit::Back),
            _ => unreachable!(),
        }
    };
    play_catalog_entries("", catalog, catalog_index, shuffle).await
}

pub(crate) fn channel_catalog(channel: &str) -> Result<Vec<TelegramCatalogEntry>> {
    let channel = normalize_channel_identity(channel);
    let label = telegram_channel_label(&channel);
    let path = telegram_catalog_path(&channel);
    let mut catalog = load_telegram_catalog(&path).with_context(|| {
        format!("no song list for {label}; sync and update the Telegram channel first")
    })?;
    for entry in &mut catalog {
        entry.channel = channel.clone();
    }
    Ok(catalog)
}

pub(crate) async fn play_catalog_entries(
    channel: &str,
    mut catalog: Vec<TelegramCatalogEntry>,
    catalog_index: usize,
    shuffle: bool,
) -> Result<PlayerExit> {
    if catalog.is_empty() {
        bail!("playlist has no songs");
    }
    let fallback_channel = normalize_channel_identity(channel);
    for entry in &mut catalog {
        if entry.channel.is_empty() {
            entry.channel = fallback_channel.clone();
        }
    }
    if catalog.iter().any(|entry| entry.channel.is_empty()) {
        bail!("a Telegram playlist track has no source channel");
    }
    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
    clear_screen()?;
    println!("Loading selected Telegram track...");
    let client = telegram_client().await?;
    let catalog_index = catalog_index.min(catalog.len() - 1);
    play_telegram_tracks(client, catalog, catalog_index, shuffle).await
}

fn choose_catalog_track(
    catalog: &[TelegramCatalogEntry],
    list_label: &str,
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
            "Search songs\r\nList: {list_label}\r\nSearch: {query}_\r\n{} match(es)\r\n\r\n",
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
            "\r\nType to search | [Ctrl+Space] toggle | [Ctrl+A] all matches | Up/Down select | [Enter] save | [Esc] cancel",
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
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !matches.is_empty() {
                    let id = catalog[matches[selected_row]].message_id;
                    if !selected.remove(&id) {
                        selected.insert(id);
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => toggle_all(
                &mut selected,
                matches.iter().map(|&index| catalog[index].message_id),
            ),
            KeyCode::Backspace => {
                query.pop();
                matches = search_catalog(catalog, &query);
                selected_row = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && !character.is_control() =>
            {
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
        .filter_map(|(index, entry)| {
            format!("{} {}", entry.channel, entry.name)
                .to_lowercase()
                .contains(&query)
                .then_some(index)
        })
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
    mut tracks: Vec<TelegramCatalogEntry>,
    mut index: usize,
    mut shuffle: bool,
) -> Result<PlayerExit> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let available_tracks = tracks.clone();
    let original_tracks = tracks.clone();
    if shuffle {
        shuffle_slice(&mut tracks);
        index = 0;
    }
    let stream = OutputStreamBuilder::open_default_stream()
        .context("failed to open default audio output")?;
    let mut volume = load_volume();
    let mut play_next = 0usize;
    let mut loop_mode = LoopMode::Off;
    let mut active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;

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
            if play_next > 0 && loop_mode != LoopMode::One {
                play_next -= 1;
            }
            match loop_mode {
                LoopMode::One => {}
                LoopMode::All => index = (index + 1) % tracks.len(),
                LoopMode::Off if index + 1 < tracks.len() => index += 1,
                LoopMode::Off => {
                    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                    return Ok(PlayerExit::Back);
                }
            }
            active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;
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
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            active.stop().await;
            delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
            return Ok(PlayerExit::Quit);
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
            KeyCode::Char('p') | KeyCode::Char(' ') => {
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
                if play_next > 0 {
                    play_next -= 1;
                }
                index = (index + 1) % tracks.len();
                active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;
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
                active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;
            }
            KeyCode::Left => {
                volume = (volume - 0.01).max(0.0);
                active.sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Right => {
                volume = (volume + 0.01).min(1.5);
                active.sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Char('l') => loop_mode = loop_mode.cycle(),
            KeyCode::Char('r') => {
                let current = tracks[index].clone();
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
                    .position(|track| track == &current)
                    .unwrap_or(0);
            }
            KeyCode::Char('u') => {
                let edit = manage_queue(
                    &mut tracks,
                    index,
                    &mut play_next,
                    &available_tracks,
                    |track| track.name.clone(),
                    |track, query| {
                        format!("{} {}", track.channel, track.name)
                            .to_lowercase()
                            .contains(&query.trim().to_lowercase())
                    },
                    || {
                        active.sink.empty()
                            && active
                                .shared
                                .state
                                .lock()
                                .map(|state| state.complete)
                                .unwrap_or(false)
                    },
                )?;
                index = edit.index;
                if edit.finished {
                    let old_cache = active.cache_path.clone();
                    active.stop().await;
                    delete_cache_file(&old_cache)?;
                    if play_next > 0 && loop_mode != LoopMode::One {
                        play_next -= 1;
                    }
                    match loop_mode {
                        LoopMode::One => {}
                        LoopMode::All => index = (index + 1) % tracks.len(),
                        LoopMode::Off if index + 1 < tracks.len() => index += 1,
                        LoopMode::Off => {
                            delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                            return Ok(PlayerExit::Back);
                        }
                    }
                    active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;
                } else if edit.restart {
                    play_next = 0;
                    let old_cache = active.cache_path.clone();
                    active.stop().await;
                    delete_cache_file(&old_cache)?;
                    active = start_catalog_track(&client, &stream, &tracks[index], volume).await?;
                }
                if edit.changed {
                    shuffle = false;
                }
            }
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.10).min(1.5);
                active.sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Down | KeyCode::Char('-') => {
                volume = (volume - 0.10).max(0.0);
                active.sink.set_volume(volume);
                save_volume(volume)?;
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
    stream: &OutputStream,
    entry: &TelegramCatalogEntry,
    volume: f32,
) -> Result<ActiveTelegramTrack> {
    let channel = &entry.channel;
    let cache_dir = Path::new(TELEGRAM_CACHE_DIR).join(channel_storage_key(channel));
    fs::create_dir_all(&cache_dir)?;
    let peer = resolve_channel(client, channel).await?;
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
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    let duration = if is_opus_path(&track.cache_path) {
        let source = OpusSource::new(reader)
            .with_context(|| format!("failed to decode Telegram track {}", track.name))?;
        let duration = source.total_duration();
        sink.append(source);
        duration
    } else {
        let decoder = Decoder::try_from(BufReader::new(reader))
            .with_context(|| format!("failed to decode Telegram track {}", track.name))?;
        let duration = decoder.total_duration();
        sink.append(decoder);
        duration
    };
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
    loop {
        let downloaded = {
            let state = shared.state.lock().map_err(lock_error)?;
            if state.cancelled {
                return Ok(());
            }
            state.downloaded
        };
        let skipped_chunks: i32 = (downloaded / DOWNLOAD_CHUNK_BYTES)
            .try_into()
            .context("Telegram track is too large to resume")?;
        let mut download = client
            .iter_download(&media)
            .chunk_size(DOWNLOAD_CHUNK_BYTES as i32)
            .skip_chunks(skipped_chunks);

        loop {
            match download.next().await {
                Ok(Some(bytes)) => {
                    if shared.state.lock().map_err(lock_error)?.cancelled {
                        return Ok(());
                    }
                    output.write_all(&bytes)?;
                    output.flush()?;
                    let mut state = shared.state.lock().map_err(lock_error)?;
                    state.downloaded += bytes.len() as u64;
                    state.reconnecting = false;
                    shared.changed.notify_all();
                }
                Ok(None) => {
                    output.flush()?;
                    let mut state = shared.state.lock().map_err(lock_error)?;
                    state.complete = true;
                    state.buffering = false;
                    state.reconnecting = false;
                    if state.total.is_none() {
                        state.total = Some(state.downloaded);
                    }
                    shared.changed.notify_all();
                    return Ok(());
                }
                Err(error) if retryable_download_error(&error) => {
                    {
                        let mut state = shared.state.lock().map_err(lock_error)?;
                        state.reconnecting = true;
                        shared.changed.notify_all();
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn retryable_download_error(error: &InvocationError) -> bool {
    matches!(
        error,
        InvocationError::Io(_) | InvocationError::Transport(_) | InvocationError::Dropped
    )
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
    } else if state.reconnecting {
        "reconnecting"
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
    let mut rows = vec![
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
        format!(
            "{} · {:.0}% · cache {} · shuffle {} · loop {}",
            playback,
            volume * 100.0,
            progress,
            if shuffle { "on" } else { "off" },
            loop_mode.label()
        ),
        String::new(),
    ];
    rows.extend(playback_controls());
    draw_panel(stdout, "Music Terminal Player · Telegram", &rows)
}

pub(crate) async fn sync_catalog(channel: &str) -> Result<()> {
    scan_channel(channel, None).await
}

pub(crate) async fn download_channel(channel: &str, folder: &Path) -> Result<()> {
    fs::create_dir_all(folder).with_context(|| format!("failed to create {}", folder.display()))?;
    scan_channel(channel, Some(folder)).await
}

pub(crate) async fn choose_private_channel(
    saved_channels: &[String],
) -> Result<Option<Vec<String>>> {
    clear_screen()?;
    println!("Scanning Telegram account dialogs...");
    let client = telegram_client().await?;
    let mut dialogs = client.iter_dialogs();
    let mut channels = Vec::new();
    let mut dialog_count = 0usize;
    while let Some(dialog) = dialogs.next().await? {
        dialog_count += 1;
        if let Peer::Channel(channel) = dialog.peer()
            && channel.username().is_none()
        {
            let label = channel.title().to_string();
            let id = dialog.peer_id().bare_id_unchecked();
            channels.push((
                label.clone(),
                id,
                private_channel_identity(dialog.peer_ref(), &label),
            ));
        }
        print!(
            "\rScanned {dialog_count} dialogs | found {} private broadcast channels",
            channels.len()
        );
        io::stdout().flush()?;
    }
    println!();
    channels.sort_by(|left, right| left.0.to_lowercase().cmp(&right.0.to_lowercase()));
    if channels.is_empty() {
        bail!("the logged-in account has no private broadcast channels");
    }

    let saved: HashSet<_> = channels
        .iter()
        .enumerate()
        .filter(|(_, (_, _, identity))| private_channel_is_saved(identity, saved_channels))
        .map(|(index, _)| index)
        .collect();
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut query = String::new();
    let mut matches: Vec<_> = (0..channels.len()).collect();
    let mut selected_row = 0usize;
    let mut selected = HashSet::new();
    loop {
        selected_row = if matches.is_empty() {
            0
        } else {
            selected_row.min(matches.len() - 1)
        };
        let start = selected_row
            .saturating_sub(PRIVATE_CHANNEL_PAGE_SIZE / 2)
            .min(matches.len().saturating_sub(PRIVATE_CHANNEL_PAGE_SIZE));
        let end = (start + PRIVATE_CHANNEL_PAGE_SIZE).min(matches.len());
        let mut frame = format!(
            "Choose private Telegram channels\r\nSearch: {query}_ | {} selected | {} matches | {} scanned\r\n\r\n",
            selected.len(),
            matches.len(),
            dialog_count
        );
        for (offset, &channel_index) in matches[start..end].iter().enumerate() {
            let (title, id, _) = &channels[channel_index];
            frame.push_str(&format!(
                "{} [{}] 🔒 {}  [channel {}]\r\n",
                if start + offset == selected_row {
                    ">"
                } else {
                    " "
                },
                if saved.contains(&channel_index) || selected.contains(&channel_index) {
                    "x"
                } else {
                    " "
                },
                title,
                id
            ));
        }
        if matches.is_empty() {
            frame.push_str("No matching private channels.\r\n");
        }
        frame.push_str(
            "\r\nType to search | [Ctrl+Space] toggle | [Ctrl+A] all matches | Up/Down/Page Up/Page Down scroll | [Enter] sync | [Esc] cancel",
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
            KeyCode::PageUp if !matches.is_empty() => {
                selected_row = selected_row.saturating_sub(PRIVATE_CHANNEL_PAGE_SIZE);
            }
            KeyCode::PageDown if !matches.is_empty() => {
                selected_row = (selected_row + PRIVATE_CHANNEL_PAGE_SIZE).min(matches.len() - 1);
            }
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !matches.is_empty() {
                    let index = matches[selected_row];
                    if !saved.contains(&index) && !selected.remove(&index) {
                        selected.insert(index);
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                toggle_all(
                    &mut selected,
                    selectable_private_channel_matches(&matches, &saved),
                );
            }
            KeyCode::Backspace => {
                query.pop();
                matches = search_private_channels(&channels, &query);
                selected_row = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && !character.is_control() =>
            {
                query.push(character);
                matches = search_private_channels(&channels, &query);
                selected_row = 0;
            }
            KeyCode::Enter => {
                if selected.is_empty() {
                    if matches.is_empty() || saved.contains(&matches[selected_row]) {
                        continue;
                    }
                    selected.insert(matches[selected_row]);
                }
                return Ok(Some(
                    channels
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| selected.contains(index))
                        .map(|(_, (_, _, identity))| identity.clone())
                        .collect(),
                ));
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn selectable_private_channel_matches(
    matches: &[usize],
    saved: &HashSet<usize>,
) -> Vec<usize> {
    matches
        .iter()
        .copied()
        .filter(|index| !saved.contains(index))
        .collect()
}

pub(crate) fn private_channel_is_saved(channel: &str, saved_channels: &[String]) -> bool {
    let Some((peer, _)) = parse_private_channel(channel) else {
        return false;
    };
    saved_channels.iter().any(|saved| {
        parse_private_channel(saved)
            .map(|(saved_peer, _)| saved_peer.id == peer.id)
            .unwrap_or(false)
    })
}

pub(crate) fn search_private_channels(
    channels: &[(String, i64, String)],
    query: &str,
) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    channels
        .iter()
        .enumerate()
        .filter(|(_, (title, id, _))| {
            query.is_empty()
                || title.to_lowercase().contains(&query)
                || id.to_string().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

pub(crate) async fn private_channel_from_link(link: &str) -> Result<String> {
    let hash = private_invite_hash(link).context("invalid private Telegram invite link")?;
    let client = telegram_client().await?;
    let invite = client
        .invoke(&tl::functions::messages::CheckChatInvite { hash })
        .await?;
    let tl::enums::ChatInvite::Already(invite) = invite else {
        bail!("this Telegram account has not joined that private channel");
    };
    let peer = Peer::from_raw(&client, invite.chat);
    let Peer::Channel(channel) = peer else {
        bail!("the invite link is not a broadcast channel");
    };
    let label = channel.title().to_string();
    let peer = channel
        .to_ref()
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .context("cannot access the private channel")?;
    Ok(private_channel_identity(peer, &label))
}

pub(crate) fn private_invite_hash(link: &str) -> Option<String> {
    let link = link.trim().trim_end_matches('/');
    let path = ["https://t.me/", "http://t.me/", "https://telegram.me/"]
        .into_iter()
        .find_map(|prefix| link.strip_prefix(prefix))?;
    let hash = path
        .strip_prefix("+")
        .or_else(|| path.strip_prefix("joinchat/"))?;
    (!hash.is_empty() && !hash.contains('/')).then(|| hash.to_string())
}

async fn scan_channel(channel: &str, download_folder: Option<&Path>) -> Result<()> {
    let started = Instant::now();
    let client = telegram_client().await?;
    let channel = normalize_channel_identity(channel);
    let label = telegram_channel_label(&channel);
    let peer = resolve_channel(&client, &channel).await?;

    let mut catalog = Vec::new();
    let mut pending = Vec::new();
    let mut message_count = 0usize;
    let mut document_count = 0usize;
    let mut messages = client.iter_messages(peer);

    println!("Scanning all songs in {label}...");
    while let Some(message) = messages.next().await? {
        message_count += 1;
        let Some(media) = message.media() else {
            continue;
        };
        if matches!(media, Media::Document(_)) {
            document_count += 1;
        }
        if !is_audio_media(&media) {
            continue;
        }

        let name =
            media_file_name(&media).unwrap_or_else(|| format!("telegram-{}.bin", message.id()));
        catalog.push(TelegramCatalogEntry {
            channel: channel.clone(),
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
    let catalog_path = telegram_catalog_path(&channel);
    if total_songs == 0 {
        bail!(
            "no supported audio found in {label}: Telegram returned {message_count} messages and {document_count} documents; the existing song list was not overwritten"
        );
    }
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

pub(crate) fn normalize_channel_identity(channel: &str) -> String {
    let channel = channel.trim();
    if channel.starts_with(PRIVATE_CHANNEL_PREFIX) {
        channel.to_string()
    } else {
        normalize_channel(channel)
    }
}

pub(crate) fn telegram_channel_label(channel: &str) -> String {
    parse_private_channel(channel)
        .map(|(_, label)| format!("🔒 {label}"))
        .unwrap_or_else(|| format!("@{}", normalize_channel(channel)))
}

pub(crate) fn private_channel_identity(peer: PeerRef, label: &str) -> String {
    let id = peer.id.bot_api_dialog_id_unchecked();
    let label = label
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{PRIVATE_CHANNEL_PREFIX}{id}:{}:{label}", peer.auth.hash())
}

fn parse_private_channel(channel: &str) -> Option<(PeerRef, String)> {
    let value = channel.strip_prefix(PRIVATE_CHANNEL_PREFIX)?;
    let mut fields = value.splitn(3, ':');
    let id = PeerId::from_bot_api_dialog_id(fields.next()?.parse().ok()?)?;
    let auth = PeerAuth::from_hash(fields.next()?.parse().ok()?);
    let encoded = fields.next()?;
    if encoded.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..encoded.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let label = String::from_utf8(bytes).ok()?;
    Some((PeerRef { id, auth }, label))
}

async fn resolve_channel(client: &Client, channel: &str) -> Result<PeerRef> {
    if channel.starts_with(PRIVATE_CHANNEL_PREFIX) {
        return parse_private_channel(channel)
            .map(|(peer, _)| peer)
            .context("invalid saved private channel; forget it and add it again");
    }
    let username = normalize_channel(channel);
    client
        .resolve_username(&username)
        .await?
        .with_context(|| format!("public channel not found: {username}"))?
        .to_ref()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .with_context(|| format!("cannot access channel: {username}"))
}

fn channel_storage_key(channel: &str) -> String {
    parse_private_channel(channel)
        .and_then(|(peer, _)| peer.id.bare_id())
        .map(|id| format!("private-{id}"))
        .unwrap_or_else(|| safe_file_name(&normalize_channel(channel)))
}

fn telegram_catalog_path(channel: &str) -> PathBuf {
    let directory = Path::new(DATA_DIR).join("catalogs");
    let path = directory.join(format!("{}.tracks.txt", channel_storage_key(channel)));
    if path.exists() {
        return path;
    }
    let Some((peer, _)) = parse_private_channel(channel) else {
        return path;
    };
    let legacy = directory.join(format!(
        "private-{}.tracks.txt",
        peer.id.bot_api_dialog_id_unchecked()
    ));
    if legacy.exists() {
        if fs::rename(&legacy, &path).is_ok() {
            return path;
        }
        return legacy;
    }
    path
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
                channel: String::new(),
                message_id: message_id.parse().with_context(|| {
                    format!("invalid message ID at {}:{}", path.display(), index + 1)
                })?,
                name: name.to_string(),
            })
        })
        .collect()
}
