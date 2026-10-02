use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use grammers_client::media::{Downloadable, Media};
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_client::{Client, SignInError};
use grammers_mtsender::{InvocationError, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use rodio::{Decoder, OutputStream, Sink, Source};
use sha2::{Digest, Sha256};
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

use crate::audio_output::{AudioOutput, device_unavailable_error};
use crate::i18n::tr;
use crate::lyrics::{Lyrics, read_embedded_lyrics};
use crate::media_controls::{MediaCommand, MediaControls};
use crate::util::{
    DATA_DIR, LoopMode, LyricsEditorResult, LyricsSettingsEditor, MAX_VOLUME, PlayerExit, RawMode,
    clear_screen, draw_frame, draw_player_panels, format_duration, format_elapsed,
    forward_track_index, is_supported_audio_path, load_setting_bool, load_volume, manage_queue,
    normalize_channel, playback_controls_with_lyrics, playback_key_bindings_hint, player_title_row,
    previous_track_index, progress_bar, prompt, restart_pass_order, safe_file_name,
    save_setting_bool, save_volume, select_menu, set_shuffle_order, shuffle_slice, toggle_all,
    toggle_playback_key_bindings,
};

pub(crate) const SESSION_FILE: &str = ".music-terminal/telegram.session";
pub(crate) const TELEGRAM_CREDENTIALS_FILE: &str = ".music-terminal/telegram.credentials";
const TELEGRAM_CACHE_DIR: &str = ".music-terminal/telegram-cache";
const TELEGRAM_RECOVERY_LOG: &str = ".music-terminal/telegram-recovery.log";
const DOWNLOAD_CHUNK_BYTES: u64 = 512 * 1024;
const FILE_MIGRATE_ERROR: i32 = 303;
const CDN_RECOVERY_RETRIES: u8 = 5;
const INITIAL_BUFFER_BYTES: u64 = 1024 * 1024;
const TRACK_LIST_PAGE_SIZE: usize = 25;
const PRIVATE_CHANNEL_PAGE_SIZE: usize = 20;
const PRIVATE_CHANNEL_PREFIX: &str = "private:";

#[derive(Clone)]
struct TelegramTrack {
    media: Media,
    size: Option<u64>,
    cache_path: PathBuf,
    peer: PeerRef,
    message_id: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TelegramCatalogEntry {
    pub(crate) channel: String,
    pub(crate) message_id: i32,
    pub(crate) published_at: i64,
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

pub(crate) fn telegram_format_hint(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .map(str::to_ascii_lowercase)
}

pub(crate) fn is_opus_path(path: &Path) -> bool {
    telegram_format_hint(path).as_deref() == Some("opus")
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
    println!("{}", tr("msg.signing_in_to_telegram"));
    'login: loop {
        let phone = loop {
            let phone = prompt(tr(
                "msg.phone_number_international_format_example_84901234567",
            ))?;
            let phone = phone.trim();
            if phone.len() > 1
                && phone.starts_with('+')
                && phone[1..].chars().all(|ch| ch.is_ascii_digit())
            {
                break phone.to_string();
            }
            println!(
                "{}",
                tr("msg.invalid_phone_number_include_and_the_country_code")
            );
        };
        let token = loop {
            match client.request_login_code(&phone, &api_hash).await {
                Ok(token) => break token,
                Err(error) => {
                    println!("{}: {error}", tr("msg.could_not_request_a_login_code"));
                    let action = prompt(tr("msg.press_enter_to_retry_type_phone_or_type"))?;
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
            let code = prompt(tr("msg.login_code"))?;
            if code.trim().is_empty() {
                println!("{}", tr("msg.login_code_cannot_be_empty"));
                continue;
            }
            match client.sign_in(&token, code.trim()).await {
                Ok(_) => break 'login,
                Err(SignInError::InvalidCode) => {
                    println!("{}", tr("msg.invalid_login_code_try_again"))
                }
                Err(SignInError::PasswordRequired(mut password_token)) => loop {
                    let hint = password_token.hint().unwrap_or("");
                    let password =
                        rpassword::prompt_password(format!("Two-step password ({hint}): "))?;
                    match client.check_password(password_token, password.trim()).await {
                        Ok(_) => break 'login,
                        Err(SignInError::InvalidPassword(next_token)) => {
                            println!("{}", tr("msg.invalid_two_step_password_try_again"));
                            password_token = next_token;
                        }
                        Err(error) => {
                            println!(
                                "{}: {error}. {}",
                                tr("msg.telegram_login_failed"),
                                tr("msg.requesting_a_new_code")
                            );
                            continue 'login;
                        }
                    }
                },
                Err(error) => {
                    println!(
                        "{}: {error}. {}",
                        tr("msg.telegram_login_failed"),
                        tr("msg.requesting_a_new_code")
                    );
                    continue 'login;
                }
            }
        }
    }
    println!(
        "{}. {} {SESSION_FILE}.",
        tr("msg.signed_in"),
        tr("msg.session_saved_in")
    );
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
        println!(
            "{}",
            tr("msg.saved_telegram_credentials_are_invalid_enter_them_again")
        );
    }

    clear_screen()?;
    println!("{}", tr("msg.telegram_setup_create_api_id_and_api_hash"));
    loop {
        let id = match prompt("Telegram api_id: ")?.trim().parse::<i32>() {
            Ok(id) if id > 0 => id,
            _ => {
                println!("{}", tr("msg.telegram_api_id_must_be_a_positive_integer"));
                continue;
            }
        };
        let hash = prompt("Telegram api_hash: ")?.trim().to_string();
        if !valid_api_credentials(id, &hash) {
            println!(
                "{}",
                tr("msg.telegram_api_hash_must_be_32_hexadecimal_characters")
            );
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
        "{}: {label}\n{} {} | {}",
        tr("msg.telegram_channel"),
        catalog.len(),
        tr("msg.tracks"),
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
        tr("msg.play_in_order").to_string(),
        tr("msg.shuffle_2").to_string(),
        tr("msg.recently_added").to_string(),
        tr("msg.search_and_choose_a_track").to_string(),
        tr("msg.search_and_choose_multiple_tracks").to_string(),
        tr("msg.back").to_string(),
    ];
    loop {
        let (tracks, play_next, shuffle) = match select_menu(title, &menu)? {
            Some(0) => (catalog.clone(), 0, false),
            Some(1) => (catalog.clone(), 0, true),
            Some(2) => {
                let mut tracks = catalog.clone();
                sort_catalog_by_published_descending(&mut tracks);
                (tracks, 0, false)
            }
            Some(3) => {
                let Some(index) = choose_catalog_track(&catalog, title)? else {
                    continue;
                };
                (prioritize_catalog_tracks(&catalog, &[index]), 0, false)
            }
            Some(4) => {
                let Some(indexes) = choose_catalog_tracks_to_play(&catalog, title)? else {
                    continue;
                };
                let play_next = indexes.len().saturating_sub(1);
                (
                    prioritize_catalog_tracks(&catalog, &indexes),
                    play_next,
                    false,
                )
            }
            Some(5) | None => return Ok(PlayerExit::Back),
            Some(_) => unreachable!(),
        };
        match play_catalog_entries("", tracks, 0, shuffle, play_next).await? {
            PlayerExit::Quit => return Ok(PlayerExit::Quit),
            PlayerExit::Back => {}
        }
    }
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
    play_next: usize,
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
    println!("{}", tr("msg.loading_selected_telegram_track"));
    let client = telegram_client().await?;
    let catalog_index = catalog_index.min(catalog.len() - 1);
    play_telegram_tracks(client, catalog, catalog_index, shuffle, play_next).await
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
            "{}\r\n{}: {list_label}\r\n{}: {query}_\r\n{} {}\r\n\r\n",
            tr("msg.search_songs"),
            tr("msg.list"),
            tr("msg.search"),
            matches.len(),
            tr("msg.match_es")
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
            frame.push_str(tr("msg.no_matching_tracks"));
        }
        frame.push_str(tr("msg.type_to_search_backspace_edit_up_down_select"));
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

fn choose_catalog_tracks_to_play(
    catalog: &[TelegramCatalogEntry],
    list_label: &str,
) -> Result<Option<Vec<usize>>> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut query = String::new();
    let mut matches: Vec<_> = (0..catalog.len()).collect();
    let mut selected_row = 0usize;
    let mut selected = HashSet::new();

    loop {
        selected_row = selected_row.min(matches.len().saturating_sub(1));
        let start = selected_row
            .saturating_sub(TRACK_LIST_PAGE_SIZE / 2)
            .min(matches.len().saturating_sub(TRACK_LIST_PAGE_SIZE));
        let end = (start + TRACK_LIST_PAGE_SIZE).min(matches.len());
        let mut frame = format!(
            "{}\r\n{}: {list_label}\r\n{}: {query}_ | {} {} | {} {}\r\n\r\n",
            tr("msg.choose_songs_to_play"),
            tr("msg.list"),
            tr("msg.search"),
            selected.len(),
            tr("msg.selected"),
            matches.len(),
            tr("msg.matches")
        );
        for (offset, &catalog_index) in matches[start..end].iter().enumerate() {
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if start + offset == selected_row {
                    ">"
                } else {
                    " "
                },
                if selected.contains(&catalog_index) {
                    "x"
                } else {
                    " "
                },
                catalog[catalog_index].name
            ));
        }
        if matches.is_empty() {
            frame.push_str(tr("msg.no_matching_tracks"));
        }
        frame.push_str(tr("msg.type_to_search_ctrl_space_toggle_ctrl_a_2"));
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
                    let index = matches[selected_row];
                    if !selected.remove(&index) {
                        selected.insert(index);
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                toggle_all(&mut selected, matches.iter().copied())
            }
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
            KeyCode::Enter if !matches.is_empty() || !selected.is_empty() => {
                if selected.is_empty() {
                    selected.insert(matches[selected_row]);
                }
                let mut indexes: Vec<_> = selected.into_iter().collect();
                indexes.sort_unstable();
                return Ok(Some(indexes));
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn prioritize_catalog_tracks<T: Clone>(catalog: &[T], selected: &[usize]) -> Vec<T> {
    let selected: HashSet<_> = selected.iter().copied().collect();
    catalog
        .iter()
        .enumerate()
        .filter(|(index, _)| selected.contains(index))
        .chain(
            catalog
                .iter()
                .enumerate()
                .filter(|(index, _)| !selected.contains(index)),
        )
        .map(|(_, track)| track.clone())
        .collect()
}
pub(crate) fn sort_catalog_by_published_descending(catalog: &mut [TelegramCatalogEntry]) {
    catalog.sort_by(|left, right| {
        right
            .published_at
            .cmp(&left.published_at)
            .then_with(|| left.channel.cmp(&right.channel))
            .then_with(|| right.message_id.cmp(&left.message_id))
    });
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
            "{}\r\n{}: {query}_ | {} {} | {} {}\r\n\r\n",
            tr("msg.choose_playlist_songs"),
            tr("msg.search"),
            selected.len(),
            tr("msg.selected"),
            matches.len(),
            tr("msg.matches")
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
            frame.push_str(tr("msg.no_matching_tracks"));
        }
        frame.push_str(tr("msg.type_to_search_ctrl_space_toggle_ctrl_a"));
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

const DECODE_WARNING: &str = "Cannot decode this track. Press p, Space, or n for next.";

struct ActiveTelegramTrack {
    sink: Sink,
    duration: Option<Duration>,
    lyrics: Option<Lyrics>,
    lyrics_checked_complete: bool,
    cache_path: PathBuf,
    shared: Arc<SharedDownload>,
    download_task: Option<JoinHandle<()>>,
    warning: Option<&'static str>,
}

impl ActiveTelegramTrack {
    async fn stop(mut self) {
        self.shared.cancel();
        self.sink.stop();
        drop(self.sink);
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
    initial_play_next: usize,
) -> Result<PlayerExit> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let available_tracks = tracks.clone();
    let original_tracks = tracks.clone();
    if shuffle {
        shuffle_slice(&mut tracks);
        index = 0;
    }
    let output = AudioOutput::open()?;
    let media_controls = MediaControls::new();
    let mut volume = load_volume();
    let mut resume_after_replay = None;
    let mut play_next = initial_play_next.min(tracks.len().saturating_sub(1));
    let mut loop_mode = LoopMode::Off;
    let mut lyrics_visible = load_setting_bool("playback.lyrics", true);
    let mut lyrics_settings = None;
    let mut lyrics_scroll = 0usize;
    let mut lyrics_index = index;
    let mut active = start_catalog_track(&client, output.stream(), &tracks[index], volume).await?;
    update_telegram_media(&media_controls, &tracks[index], &active);

    loop {
        if lyrics_index != index {
            lyrics_scroll = 0;
            lyrics_index = index;
        }
        if output.is_lost() {
            active.stop().await;
            delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
            return Err(device_unavailable_error());
        }

        if let Some(command) = media_controls.command() {
            match command {
                MediaCommand::Play if active.warning.is_none() => {
                    active.sink.play();
                    media_controls.set_playing(true);
                }
                MediaCommand::Play => {}
                MediaCommand::Pause => {
                    active.sink.pause();
                    media_controls.set_playing(false);
                }
                MediaCommand::Next | MediaCommand::Previous => {
                    let forward = command == MediaCommand::Next;
                    let next = if forward {
                        forward_track_index(
                            index,
                            tracks.len(),
                            loop_mode,
                            false,
                            &mut resume_after_replay,
                        )
                    } else {
                        Some((
                            previous_track_index(index, tracks.len(), &mut resume_after_replay),
                            false,
                            false,
                        ))
                    };
                    let Some((next, new_pass, advance_queue)) = next else {
                        active.stop().await;
                        delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                        return Ok(PlayerExit::Back);
                    };
                    let old_cache = active.cache_path.clone();
                    active.stop().await;
                    delete_cache_file(&old_cache)?;
                    if new_pass {
                        restart_pass_order(&mut tracks, &original_tracks, shuffle);
                    }
                    if advance_queue && play_next > 0 {
                        play_next -= 1;
                    }
                    index = next;
                    active = start_catalog_track(&client, output.stream(), &tracks[index], volume)
                        .await?;
                    update_telegram_media(&media_controls, &tracks[index], &active);
                }
            }
            continue;
        }

        let complete = active.shared.state.lock().map_err(lock_error)?.complete;
        if complete && !active.lyrics_checked_complete {
            active.lyrics = read_embedded_lyrics(&active.cache_path).ok().flatten();
            active.lyrics_checked_complete = true;
        }

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
            active.warning,
            active.lyrics.as_ref(),
            lyrics_scroll,
            lyrics_visible,
            lyrics_settings.as_ref(),
        )?;

        if active.warning.is_none() && complete && active.sink.empty() {
            let Some((next, new_pass, advance_queue)) = forward_track_index(
                index,
                tracks.len(),
                loop_mode,
                true,
                &mut resume_after_replay,
            ) else {
                active.stop().await;
                delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                return Ok(PlayerExit::Back);
            };
            let old_cache = active.cache_path.clone();
            active.stop().await;
            delete_cache_file(&old_cache)?;
            if new_pass {
                restart_pass_order(&mut tracks, &original_tracks, shuffle);
            }
            if advance_queue && play_next > 0 && loop_mode != LoopMode::One {
                play_next -= 1;
            }
            index = next;
            active = start_catalog_track(&client, output.stream(), &tracks[index], volume).await?;
            update_telegram_media(&media_controls, &tracks[index], &active);
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

        if let Some(editor) = lyrics_settings.as_mut() {
            match editor.handle_key(key.code, key.modifiers, lyrics_visible)? {
                LyricsEditorResult::Visibility(visible) => {
                    lyrics_visible = visible;
                    continue;
                }
                LyricsEditorResult::Close => {
                    lyrics_settings = None;
                    continue;
                }
                LyricsEditorResult::Handled => continue,
                LyricsEditorResult::Pass => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => {
                active.stop().await;
                delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                return Ok(PlayerExit::Quit);
            }
            KeyCode::Char('b') => {
                lyrics_settings = Some(LyricsSettingsEditor::new());
            }
            KeyCode::Char('/') => toggle_playback_key_bindings()?,
            KeyCode::Esc => {
                active.stop().await;
                delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                return Ok(PlayerExit::Back);
            }
            KeyCode::Char('p') | KeyCode::Char(' ') if active.warning.is_none() => {
                if active.sink.is_paused() {
                    active.sink.play();
                    media_controls.set_playing(true);
                } else {
                    active.sink.pause();
                    media_controls.set_playing(false);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('p') | KeyCode::Char(' ') => {
                let Some((next, new_pass, advance_queue)) = forward_track_index(
                    index,
                    tracks.len(),
                    loop_mode,
                    false,
                    &mut resume_after_replay,
                ) else {
                    active.stop().await;
                    delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                    return Ok(PlayerExit::Back);
                };
                let old_cache = active.cache_path.clone();
                active.stop().await;
                delete_cache_file(&old_cache)?;
                if new_pass {
                    restart_pass_order(&mut tracks, &original_tracks, shuffle);
                }
                if advance_queue && play_next > 0 {
                    play_next -= 1;
                }
                index = next;
                active =
                    start_catalog_track(&client, output.stream(), &tracks[index], volume).await?;
                update_telegram_media(&media_controls, &tracks[index], &active);
            }
            KeyCode::Char('v') if key.modifiers.is_empty() => {
                let old_cache = active.cache_path.clone();
                active.stop().await;
                delete_cache_file(&old_cache)?;
                index = previous_track_index(index, tracks.len(), &mut resume_after_replay);
                active =
                    start_catalog_track(&client, output.stream(), &tracks[index], volume).await?;
                update_telegram_media(&media_controls, &tracks[index], &active);
            }
            KeyCode::Left => {
                volume = (volume - 0.01).max(0.0);
                active.sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Right => {
                volume = (volume + 0.01).min(MAX_VOLUME);
                active.sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Char('l') => loop_mode = loop_mode.cycle(),
            KeyCode::Char('y') => {
                lyrics_visible = !lyrics_visible;
                save_setting_bool("playback.lyrics", lyrics_visible)?;
            }
            KeyCode::PageUp if lyrics_visible => {
                lyrics_scroll = lyrics_scroll.saturating_sub(5);
            }
            KeyCode::PageDown if lyrics_visible => {
                lyrics_scroll = lyrics_scroll.saturating_add(5);
            }
            KeyCode::Char('r') => {
                resume_after_replay = None;
                shuffle = !shuffle;
                set_shuffle_order(&mut tracks, index, play_next, &original_tracks, shuffle);
            }
            KeyCode::Char('u') => loop {
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
                        active.warning.is_none()
                            && active.sink.empty()
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
                    let Some((next, new_pass, advance_queue)) = forward_track_index(
                        index,
                        tracks.len(),
                        loop_mode,
                        true,
                        &mut resume_after_replay,
                    ) else {
                        active.stop().await;
                        delete_cache_directory(Path::new(TELEGRAM_CACHE_DIR))?;
                        return Ok(PlayerExit::Back);
                    };
                    let old_cache = active.cache_path.clone();
                    active.stop().await;
                    delete_cache_file(&old_cache)?;
                    if new_pass {
                        restart_pass_order(&mut tracks, &original_tracks, shuffle);
                    }
                    if advance_queue && play_next > 0 && loop_mode != LoopMode::One {
                        play_next -= 1;
                    }
                    index = next;
                    active = start_catalog_track(&client, output.stream(), &tracks[index], volume)
                        .await?;
                    update_telegram_media(&media_controls, &tracks[index], &active);
                    continue;
                }
                if edit.restart {
                    resume_after_replay = None;
                    play_next = 0;
                    let old_cache = active.cache_path.clone();
                    active.stop().await;
                    delete_cache_file(&old_cache)?;
                    active = start_catalog_track(&client, output.stream(), &tracks[index], volume)
                        .await?;
                    update_telegram_media(&media_controls, &tracks[index], &active);
                }
                break;
            },
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.10).min(MAX_VOLUME);
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

fn update_telegram_media(
    media_controls: &MediaControls,
    track: &TelegramCatalogEntry,
    active: &ActiveTelegramTrack,
) {
    media_controls.set_track(&track.name, &track.channel);
    media_controls.set_playing(active.warning.is_none() && !active.sink.is_paused());
}

fn delete_cache_file(path: &Path) -> Result<()> {
    for attempt in 0..40 {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt < 39 => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to delete cache file {}", path.display()));
            }
        }
    }
    unreachable!()
}

pub(crate) fn delete_cache_directory(path: &Path) -> Result<()> {
    for attempt in 0..40 {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt < 39 => std::thread::sleep(Duration::from_millis(100)),
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
        .get_messages_by_id(peer.clone(), &[entry.message_id])
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
        size: media.size().map(|size| size as u64),
        cache_path: cache_dir.join(format!(
            "{}-{}",
            entry.message_id,
            safe_file_name(&entry.name)
        )),
        media,
        peer,
        message_id: entry.message_id,
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
        let peer = track.peer.clone();
        let message_id = track.message_id;
        let path = track.cache_path.clone();
        let download = Arc::clone(&shared);
        download_task = Some(tokio::spawn(async move {
            if let Err(error) =
                download_progressively(client, media, peer, message_id, path, Arc::clone(&download))
                    .await
                && let Ok(mut state) = download.state.lock()
            {
                state.error = Some(error.to_string());
                state.buffering = false;
                download.changed.notify_all();
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
    let lyrics = read_embedded_lyrics(&track.cache_path).ok().flatten();
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    let mut warning = None;
    let duration = if is_opus_path(&track.cache_path) {
        match OpusSource::new(reader) {
            Ok(source) => {
                let duration = source.total_duration();
                sink.append(source);
                duration
            }
            Err(_) => {
                warning = Some(DECODE_WARNING);
                None
            }
        }
    } else {
        let hint = telegram_format_hint(&track.cache_path);
        let decoder = match telegram_decoder(reader, track.size, hint.as_deref()) {
            Ok(decoder) => Some(decoder),
            Err(_) if !cache_complete => {
                wait_for_download(&shared).await?;
                let reader = ProgressiveReader {
                    file: open_cache_for_read(&track.cache_path)?,
                    position: 0,
                    shared: Arc::clone(&shared),
                };
                telegram_decoder(reader, track.size, hint.as_deref()).ok()
            }
            Err(_) => None,
        };
        decoder.map_or_else(
            || {
                warning = Some(DECODE_WARNING);
                None
            },
            |decoder| {
                let duration = decoder.total_duration();
                sink.append(decoder);
                duration
            },
        )
    };
    Ok(ActiveTelegramTrack {
        sink,
        duration,
        lyrics,
        lyrics_checked_complete: cache_complete,
        cache_path: track.cache_path.clone(),
        shared,
        download_task,
        warning,
    })
}

fn telegram_decoder(
    reader: ProgressiveReader,
    byte_len: Option<u64>,
    hint: Option<&str>,
) -> Result<Decoder<BufReader<ProgressiveReader>>, rodio::decoder::DecoderError> {
    let mut builder = Decoder::builder().with_data(BufReader::new(reader));
    if let Some(byte_len) = byte_len {
        builder = builder.with_byte_len(byte_len);
    }
    if let Some(hint) = hint {
        builder = builder.with_hint(hint);
    }
    builder.build()
}

async fn wait_for_download(shared: &SharedDownload) -> Result<()> {
    loop {
        {
            let state = shared.state.lock().map_err(lock_error)?;
            if let Some(error) = &state.error {
                bail!("Telegram download failed: {error}");
            }
            if state.cancelled {
                bail!("Telegram download was cancelled");
            }
            if state.complete {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn download_progressively(
    client: Client,
    mut media: Media,
    peer: PeerRef,
    message_id: i32,
    path: PathBuf,
    shared: Arc<SharedDownload>,
) -> Result<()> {
    let mut output = open_cache_for_write(&path)?;
    let mut retry_offset = None;
    let mut retry_count = 0u8;
    let mut chunk_bytes = DOWNLOAD_CHUNK_BYTES;
    loop {
        let downloaded = {
            let state = shared.state.lock().map_err(lock_error)?;
            if state.cancelled {
                return Ok(());
            }
            state.downloaded
        };
        if downloaded % chunk_bytes != 0 {
            bail!(
                "Telegram resume offset {downloaded} is not aligned to the active chunk size {chunk_bytes}"
            );
        }
        let skipped_chunks: i32 = (downloaded / chunk_bytes)
            .try_into()
            .context("Telegram track is too large to resume")?;
        let mut download = client
            .iter_download(&media)
            .chunk_size(chunk_bytes as i32)
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
                    retry_offset = None;
                    retry_count = 0;
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
                Err(error) if download_retry_delay(&error).is_some() => {
                    if retry_offset == Some(downloaded) {
                        retry_count = retry_count.saturating_add(1);
                    } else {
                        retry_offset = Some(downloaded);
                        retry_count = 1;
                    }
                    if retry_count >= CDN_RECOVERY_RETRIES {
                        {
                            let mut state = shared.state.lock().map_err(lock_error)?;
                            state.reconnecting = true;
                            shared.changed.notify_all();
                        }
                        match recover_download_chunk(
                            &client,
                            &peer,
                            message_id,
                            &media,
                            downloaded,
                            chunk_bytes,
                        )
                        .await
                        {
                            Ok(recovered) if !recovered.bytes.is_empty() => {
                                output.write_all(&recovered.bytes)?;
                                output.flush()?;
                                media = recovered.media;
                                chunk_bytes = recovered.chunk_bytes.min(DOWNLOAD_CHUNK_BYTES);
                                let mut state = shared.state.lock().map_err(lock_error)?;
                                state.downloaded += recovered.bytes.len() as u64;
                                state.reconnecting = false;
                                let recovered_complete = recovered.reached_end
                                    || state.total.is_some_and(|total| state.downloaded >= total);
                                if recovered_complete {
                                    state.complete = true;
                                    state.buffering = false;
                                }
                                shared.changed.notify_all();
                                drop(state);
                                retry_offset = None;
                                retry_count = 0;
                                if recovered_complete {
                                    return Ok(());
                                }
                                break;
                            }
                            Ok(_) => {
                                bail!(
                                    "Telegram recovery returned no data at byte offset {downloaded}"
                                );
                            }
                            Err(recovery_error) => {
                                bail!(
                                    "Telegram cannot read this file at byte offset {downloaded} after {retry_count} retries ({error}); Telegram-only recovery also failed: {recovery_error}"
                                );
                            }
                        }
                    }
                    let delay = download_retry_delay(&error).unwrap_or(Duration::from_secs(2));
                    {
                        let mut state = shared.state.lock().map_err(lock_error)?;
                        state.reconnecting = true;
                        shared.changed.notify_all();
                    }
                    tokio::time::sleep(delay).await;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

struct RecoveredTelegramChunk {
    bytes: Vec<u8>,
    media: Media,
    chunk_bytes: u64,
    reached_end: bool,
}

async fn refresh_download_media(client: &Client, peer: &PeerRef, message_id: i32) -> Result<Media> {
    let message = client
        .get_messages_by_id(peer.clone(), &[message_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .context("Telegram message disappeared while refreshing the download")?;
    let media = message
        .media()
        .context("Telegram message no longer contains downloadable media")?;
    if !is_audio_media(&media) {
        bail!("Telegram message no longer contains audio media");
    }
    Ok(media)
}

fn append_recovery_diagnostic(message: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(TELEGRAM_RECOVERY_LOG)
    {
        let _ = writeln!(file, "{message}");
    }
}

const TELEGRAM_FILE_WINDOW_BYTES: u64 = 1024 * 1024;
// Match grammers-client's normal sequential downloader. Its public downloader
// caps upload.getFile requests at 512 KiB, which also stays comfortably below
// grammers-mtproto's message-container ceiling once MTProto/TL overhead is added.
const MAX_SAFE_RECOVERY_CHUNK_BYTES: u64 = 512 * 1024;
const MIN_RECOVERY_CHUNK_BYTES: u64 = 4 * 1024;

fn recovery_chunk_sizes(preferred: u64, offset: u64) -> Vec<u64> {
    const CANDIDATE_KIB: &[u64] = &[512, 256, 128, 64, 32, 16, 8, 4];

    let offset_in_window = offset % TELEGRAM_FILE_WINDOW_BYTES;
    let window_remaining = TELEGRAM_FILE_WINDOW_BYTES - offset_in_window;
    let max_chunk = preferred
        .max(MIN_RECOVERY_CHUNK_BYTES)
        .min(MAX_SAFE_RECOVERY_CHUNK_BYTES)
        .min(window_remaining);

    CANDIDATE_KIB
        .iter()
        .map(|kib| kib * 1024)
        .filter(|size| *size <= max_chunk && offset % *size == 0)
        .collect()
}

fn media_dc_id(media: &Media) -> Option<i32> {
    match media {
        Media::Document(document) => match document.raw.document.as_ref()? {
            tl::enums::Document::Document(raw) => Some(raw.dc_id),
            _ => None,
        },
        _ => None,
    }
}

async fn download_direct_recovery_chunk(
    client: &Client,
    media: &Media,
    offset: u64,
    chunk_bytes: u64,
) -> Result<Vec<u8>> {
    if offset % MIN_RECOVERY_CHUNK_BYTES != 0
        || chunk_bytes % MIN_RECOVERY_CHUNK_BYTES != 0
        || chunk_bytes > MAX_SAFE_RECOVERY_CHUNK_BYTES
        || offset % chunk_bytes != 0
    {
        bail!("Telegram recovery range does not satisfy upload.getFile alignment rules");
    }
    let end = offset
        .checked_add(chunk_bytes.saturating_sub(1))
        .context("Telegram recovery range overflow")?;
    if offset / TELEGRAM_FILE_WINDOW_BYTES != end / TELEGRAM_FILE_WINDOW_BYTES {
        bail!("Telegram recovery range crosses a 1 MiB boundary");
    }

    let chunk_size: i32 = chunk_bytes
        .try_into()
        .context("Telegram recovery chunk is too large")?;
    let chunk_index: i32 = (offset / chunk_bytes)
        .try_into()
        .context("Telegram recovery offset is too large")?;
    let mut download = client
        .iter_download(media)
        .chunk_size(chunk_size)
        .skip_chunks(chunk_index);

    match download.next().await {
        Ok(bytes) => Ok(bytes.unwrap_or_default()),
        Err(primary_error) if chunk_bytes == MAX_SAFE_RECOVERY_CHUNK_BYTES => {
            match download_cdn_bootstrap_chunk(client, media, offset, chunk_bytes).await {
                Ok(bytes) => Ok(bytes),
                Err(cdn_error) => bail!(
                    "sequential request failed: {primary_error}; CDN bootstrap recovery failed: {cdn_error:#}"
                ),
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn download_cdn_bootstrap_chunk(
    client: &Client,
    media: &Media,
    offset: u64,
    chunk_bytes: u64,
) -> Result<Vec<u8>> {
    let location = media
        .to_raw_input_location()
        .context("Telegram media has no raw file location")?;
    let media_size = media.size().map(|size| size as u64);
    let mut probe_offsets = vec![
        0,
        offset.saturating_sub(TELEGRAM_FILE_WINDOW_BYTES),
        offset.saturating_add(TELEGRAM_FILE_WINDOW_BYTES),
    ];
    probe_offsets.sort_unstable();
    probe_offsets.dedup();
    if let Some(size) = media_size {
        probe_offsets.retain(|probe| *probe < size);
    }

    let mut errors = Vec::new();
    for probe_offset in probe_offsets {
        let mut master_dc = media_dc_id(media).context("Telegram media has no document DC")?;
        let request = tl::functions::upload::GetFile {
            precise: false,
            cdn_supported: true,
            location: location.clone(),
            offset: probe_offset as i64,
            limit: MAX_SAFE_RECOVERY_CHUNK_BYTES as i32,
        };

        loop {
            match client.invoke_in_dc(master_dc, &request).await {
                Ok(tl::enums::upload::File::CdnRedirect(redirect)) => {
                    append_recovery_diagnostic(&format!(
                        "offset={offset} CDN bootstrap succeeded via probe={probe_offset} master_dc={master_dc} cdn_dc={}",
                        redirect.dc_id
                    ));
                    return download_cdn_chunk(client, master_dc, &redirect, offset, chunk_bytes)
                        .await;
                }
                Ok(tl::enums::upload::File::File(_)) => {
                    errors.push(format!(
                        "probe {probe_offset}: master DC returned file bytes"
                    ));
                    break;
                }
                Err(InvocationError::Rpc(error)) if error.code == FILE_MIGRATE_ERROR => {
                    master_dc = error
                        .value
                        .context("FILE_MIGRATE missing target DC")?
                        .try_into()
                        .context("FILE_MIGRATE returned an invalid target DC")?;
                }
                Err(error) => {
                    errors.push(format!("probe {probe_offset}: {error}"));
                    break;
                }
            }
        }
    }

    bail!(
        "Telegram did not provide a usable CDN redirect: {}",
        errors.join(" | ")
    )
}

async fn download_cdn_chunk(
    client: &Client,
    master_dc: i32,
    redirect: &tl::types::upload::FileCdnRedirect,
    offset: u64,
    chunk_bytes: u64,
) -> Result<Vec<u8>> {
    let request = tl::functions::upload::GetCdnFile {
        file_token: redirect.file_token.clone(),
        offset: offset as i64,
        limit: chunk_bytes as i32,
    };

    let encrypted = loop {
        match client.invoke_in_dc(redirect.dc_id, &request).await? {
            tl::enums::upload::CdnFile::File(file) => break file.bytes,
            tl::enums::upload::CdnFile::ReuploadNeeded(needed) => {
                client
                    .invoke_in_dc(
                        master_dc,
                        &tl::functions::upload::ReuploadCdnFile {
                            file_token: redirect.file_token.clone(),
                            request_token: needed.request_token,
                        },
                    )
                    .await?;
            }
        }
    };

    let mut decrypted = encrypted;
    decrypt_cdn_bytes(
        &redirect.encryption_key,
        &redirect.encryption_iv,
        offset,
        &mut decrypted,
    )?;

    let mut hashes = redirect.file_hashes.clone();
    if !cdn_hashes_cover(&hashes, offset, decrypted.len()) {
        hashes = client
            .invoke_in_dc(
                master_dc,
                &tl::functions::upload::GetCdnFileHashes {
                    file_token: redirect.file_token.clone(),
                    offset: offset as i64,
                },
            )
            .await?;
    }
    verify_cdn_hashes(&hashes, offset, &decrypted)?;
    Ok(decrypted)
}

fn decrypt_cdn_bytes(key: &[u8], iv: &[u8], offset: u64, bytes: &mut [u8]) -> Result<()> {
    if key.len() != 32 || iv.len() != 16 {
        bail!("Telegram CDN returned an invalid AES-256 key or IV");
    }
    let counter = u32::try_from(offset / 16).context("Telegram CDN offset is too large")?;
    let mut adjusted_iv = iv.to_vec();
    adjusted_iv[12..16].copy_from_slice(&counter.to_be_bytes());
    let mut cipher = ctr::Ctr128BE::<aes::Aes256>::new_from_slices(key, &adjusted_iv)
        .map_err(|_| anyhow::anyhow!("Telegram CDN AES-CTR initialization failed"))?;
    cipher.apply_keystream(bytes);
    Ok(())
}

fn cdn_hashes_cover(hashes: &[tl::enums::FileHash], offset: u64, len: usize) -> bool {
    let end = offset.saturating_add(len as u64);
    let mut cursor = offset;
    while cursor < end {
        let Some(hash) = hashes.iter().find_map(|hash| match hash {
            tl::enums::FileHash::Hash(hash) if hash.offset == cursor as i64 => Some(hash),
            _ => None,
        }) else {
            return false;
        };
        if hash.limit <= 0 {
            return false;
        }
        cursor = cursor.saturating_add(hash.limit as u64);
    }
    cursor == end
}

fn verify_cdn_hashes(hashes: &[tl::enums::FileHash], offset: u64, bytes: &[u8]) -> Result<()> {
    let end = offset.saturating_add(bytes.len() as u64);
    let mut cursor = offset;
    while cursor < end {
        let hash = hashes
            .iter()
            .find_map(|hash| match hash {
                tl::enums::FileHash::Hash(hash) if hash.offset == cursor as i64 => Some(hash),
                _ => None,
            })
            .with_context(|| format!("Telegram CDN hash missing at byte offset {cursor}"))?;
        let limit: usize = hash
            .limit
            .try_into()
            .context("Telegram CDN hash has invalid length")?;
        let relative: usize = (cursor - offset)
            .try_into()
            .context("Telegram CDN hash offset is too large")?;
        let part = bytes
            .get(relative..relative.saturating_add(limit))
            .context("Telegram CDN hash range exceeds downloaded data")?;
        let digest = Sha256::digest(part);
        if digest.as_slice() != hash.hash.as_slice() {
            bail!("Telegram CDN SHA-256 mismatch at byte offset {cursor}");
        }
        cursor = cursor.saturating_add(limit as u64);
    }
    if cursor != end {
        bail!("Telegram CDN hash coverage does not match downloaded data");
    }
    Ok(())
}

fn partial_download_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

async fn download_media_resilient(client: &Client, media: &Media, dest: &Path) -> Result<()> {
    let expected_size = media
        .size()
        .map(|size| size as u64)
        .context("Telegram media does not report a file size")?;
    let part_path = partial_download_path(dest);
    let mut downloaded = fs::metadata(&part_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    if downloaded > expected_size || (downloaded < expected_size && downloaded % 4096 != 0) {
        let _ = fs::remove_file(&part_path);
        downloaded = 0;
    }

    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .with_context(|| format!("failed to open {}", part_path.display()))?;

    while downloaded < expected_size {
        let mut recovered = None;
        let mut errors = Vec::new();
        for chunk_bytes in recovery_chunk_sizes(1024 * 1024, downloaded) {
            match download_direct_recovery_chunk(client, media, downloaded, chunk_bytes).await {
                Ok(bytes) if !bytes.is_empty() => {
                    recovered = Some((bytes, chunk_bytes));
                    break;
                }
                Ok(_) => errors.push(format!("{} KiB: empty response", chunk_bytes / 1024)),
                Err(error) => errors.push(format!("{} KiB: {error:#}", chunk_bytes / 1024)),
            }
        }

        let (bytes, requested) = recovered.with_context(|| {
            format!(
                "Telegram media-DC download failed at byte offset {downloaded}: {}",
                errors.join(" | ")
            )
        })?;
        if bytes.len() > requested as usize {
            bail!("Telegram returned more data than requested");
        }
        output.write_all(&bytes)?;
        output.flush()?;
        downloaded = downloaded.saturating_add(bytes.len() as u64);
        if downloaded > expected_size {
            bail!("Telegram returned more bytes than the declared media size");
        }
        if bytes.len() < requested as usize && downloaded < expected_size && downloaded % 4096 != 0
        {
            bail!(
                "Telegram returned a short non-final chunk ending at unaligned byte offset {downloaded}"
            );
        }
    }

    output.sync_all()?;
    drop(output);
    fs::rename(&part_path, dest).with_context(|| {
        format!(
            "failed to finalize Telegram download {} -> {}",
            part_path.display(),
            dest.display()
        )
    })?;
    Ok(())
}

async fn recover_download_chunk(
    client: &Client,
    peer: &PeerRef,
    message_id: i32,
    current_media: &Media,
    offset: u64,
    preferred_chunk_bytes: u64,
) -> Result<RecoveredTelegramChunk> {
    let refreshed_media = match refresh_download_media(client, peer, message_id).await {
        Ok(media) => media,
        Err(error) => {
            append_recovery_diagnostic(&format!("offset={offset} refresh_media failed: {error:#}"));
            current_media.clone()
        }
    };
    let dc_id = media_dc_id(&refreshed_media);
    let chunk_sizes = recovery_chunk_sizes(preferred_chunk_bytes, offset);
    let mut errors = Vec::new();

    for chunk_bytes in chunk_sizes.iter().copied() {
        match download_direct_recovery_chunk(client, &refreshed_media, offset, chunk_bytes).await {
            Ok(bytes) if !bytes.is_empty() => {
                let reached_end = bytes.len() < chunk_bytes as usize;
                return Ok(RecoveredTelegramChunk {
                    bytes,
                    media: refreshed_media,
                    chunk_bytes,
                    reached_end,
                });
            }
            Ok(_) => errors.push(format!("{} KiB: empty response", chunk_bytes / 1024)),
            Err(error) => {
                let detail = format!("{} KiB: {error:#}", chunk_bytes / 1024);
                append_recovery_diagnostic(&format!(
                    "offset={offset} dc={} {detail}",
                    dc_id.map_or_else(|| "unknown".to_string(), |dc| dc.to_string())
                ));
                errors.push(detail);
            }
        }
    }

    bail!(
        "master media DC recovery failed at byte offset {offset}: {}",
        errors.join(" | ")
    )
}
#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn cdn_ctr_round_trip_respects_offset() {
        let key = [0x42u8; 32];
        let iv = [0x24u8; 16];
        let offset = 512 * 1024;
        let original = b"telegram cdn recovery".to_vec();
        let mut encrypted = original.clone();
        decrypt_cdn_bytes(&key, &iv, offset, &mut encrypted).unwrap();
        assert_ne!(encrypted, original);
        decrypt_cdn_bytes(&key, &iv, offset, &mut encrypted).unwrap();
        assert_eq!(encrypted, original);
    }

    #[test]
    fn cdn_hash_verification_accepts_exact_coverage_and_rejects_tampering() {
        let bytes = b"0123456789abcdef";
        let hash = tl::enums::FileHash::Hash(tl::types::FileHash {
            offset: 4096,
            limit: bytes.len() as i32,
            hash: Sha256::digest(bytes).to_vec(),
        });
        let hashes = vec![hash];
        assert!(cdn_hashes_cover(&hashes, 4096, bytes.len()));
        verify_cdn_hashes(&hashes, 4096, bytes).unwrap();

        let mut tampered = bytes.to_vec();
        tampered[0] ^= 1;
        assert!(verify_cdn_hashes(&hashes, 4096, &tampered).is_err());
    }

    #[test]
    fn recovery_chunk_sizes_step_down_without_losing_alignment() {
        assert_eq!(
            recovery_chunk_sizes(1024 * 1024, 72 * 1024 * 1024),
            vec![
                512 * 1024,
                256 * 1024,
                128 * 1024,
                64 * 1024,
                32 * 1024,
                16 * 1024,
                8 * 1024,
                4 * 1024
            ]
        );
        assert_eq!(
            recovery_chunk_sizes(128 * 1024, 72 * 1024 * 1024 + 64 * 1024),
            vec![64 * 1024, 32 * 1024, 16 * 1024, 8 * 1024, 4 * 1024]
        );
        assert_eq!(
            recovery_chunk_sizes(1024 * 1024, 72 * 1024 * 1024 + 512 * 1024),
            vec![
                512 * 1024,
                256 * 1024,
                128 * 1024,
                64 * 1024,
                32 * 1024,
                16 * 1024,
                8 * 1024,
                4 * 1024
            ]
        );
        assert!(
            recovery_chunk_sizes(1024 * 1024, 72 * 1024 * 1024)
                .into_iter()
                .all(|size| size < 1_044_448)
        );
    }
}

fn download_retry_delay(error: &InvocationError) -> Option<Duration> {
    match error {
        InvocationError::Io(_) | InvocationError::Transport(_) | InvocationError::Dropped => {
            Some(Duration::from_secs(2))
        }
        InvocationError::Rpc(error) if error.code == 420 => Some(Duration::from_secs(
            error.value.unwrap_or(2).clamp(1, 60) as u64,
        )),
        InvocationError::Rpc(error)
            if error.code >= 500
                || error.code <= -500
                || error.name.eq_ignore_ascii_case("Timeout") =>
        {
            Some(Duration::from_secs(2))
        }
        _ => None,
    }
}

fn open_cache_for_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.share_mode(0x1 | 0x2 | 0x4);
    options.open(path)
}

pub(crate) fn open_cache_for_write(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(windows)]
    options.share_mode(0x1 | 0x2 | 0x4);
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
    warning: Option<&str>,
    lyrics: Option<&Lyrics>,
    lyrics_scroll: usize,
    lyrics_visible: bool,
    lyrics_settings: Option<&LyricsSettingsEditor>,
) -> Result<()> {
    let state = shared.state.lock().map_err(lock_error)?;
    let playback = if warning.is_some() || state.error.is_some() {
        tr("msg.cannot_play")
    } else if sink.is_paused() {
        tr("msg.paused")
    } else if state.reconnecting {
        tr("msg.reconnecting")
    } else if state.buffering {
        tr("msg.buffering")
    } else {
        tr("msg.playing")
    };
    let progress = state
        .total
        .filter(|total| *total > 0)
        .map(|total| {
            format!(
                "{:.0}%",
                (state.downloaded as f64 * 100.0 / total as f64).min(100.0)
            )
        })
        .unwrap_or_else(|| format!("{} KiB", state.downloaded / 1024));
    let elapsed = sink.get_pos();
    let total = duration
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let mut rows = vec![
        player_title_row(tr("msg.track"), index, tracks.len(), &tracks[index].name),
        String::new(),
        format!(
            "[{}] {}/{}",
            progress_bar(elapsed, duration, 12),
            format_duration(elapsed),
            total
        ),
        format!(
            "{} · {:.0}% · cache {} · {} {} · {} {}",
            playback,
            volume * 100.0,
            progress,
            tr("msg.shuffle"),
            if shuffle { tr("msg.on") } else { tr("msg.off") },
            tr("msg.loop"),
            loop_mode.label()
        ),
        String::new(),
    ];
    if let Some(warning) = warning {
        rows.push(format!("{}: {warning}", tr("msg.warning")));
        rows.push(String::new());
    } else if let Some(error) = state.error.as_deref() {
        rows.push(format!(
            "{}: Telegram download failed: {error}",
            tr("msg.warning")
        ));
        rows.push(String::new());
    }
    if load_setting_bool("playback.key_bindings", true) {
        rows.push(playback_key_bindings_hint());
        rows.extend(playback_controls_with_lyrics());
    }
    draw_player_panels(
        stdout,
        "Music Terminal Player · Telegram",
        &rows,
        lyrics,
        elapsed,
        lyrics_scroll,
        lyrics_visible,
        lyrics_settings,
    )
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
    println!("{}", tr("msg.scanning_telegram_account_dialogs"));
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
    channels.sort_by_key(|channel| channel.0.to_lowercase());
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
                "{} [{}] {}{}  [channel {}]\r\n",
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
                if load_setting_bool("telegram.channel_icons", true) {
                    "🔒 "
                } else {
                    ""
                },
                title,
                id
            ));
        }
        if matches.is_empty() {
            frame.push_str(tr("msg.no_matching_private_channels"));
        }
        frame.push_str(
            "\r\nType to search | [Ctrl+Space] toggle | [Ctrl+A] all matches | ↑/↓ scroll | [Enter] sync | [Esc] cancel",
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
    let mut messages = client.iter_messages(peer.clone());

    println!("{} {label}...", tr("msg.scanning_all_songs_in"));
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
            published_at: message.date().timestamp(),
            name: name.clone(),
        });
        if let Some(folder) = download_folder {
            let safe_name = safe_file_name(&name);
            let dest = folder.join(format!("{}-{}", message.id(), safe_name));
            if !dest.exists() {
                pending.push((message.id(), media, dest));
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
        println!("{}", tr("msg.song_list_update_complete"));
        return Ok(());
    };

    let download_total = pending.len();
    let mut failed_downloads = 0usize;
    for (index, (message_id, media, dest)) in pending.into_iter().enumerate() {
        println!(
            "[{}/{}] Downloading {} | download time {}",
            index + 1,
            download_total,
            dest.display(),
            format_elapsed(started.elapsed().saturating_sub(scan_time))
        );
        let media = refresh_download_media(&client, &peer, message_id)
            .await
            .unwrap_or(media);
        if let Err(error) = download_media_resilient(&client, &media, &dest).await {
            failed_downloads += 1;
            let part_path = partial_download_path(&dest);
            eprintln!(
                "[FAILED] {} | {error:#} | partial kept at {}",
                dest.display(),
                part_path.display()
            );
        }
    }

    let downloaded = download_total.saturating_sub(failed_downloads);
    if failed_downloads == 0 {
        println!(
            "Download complete. Channel songs: {total_songs}, downloaded: {downloaded}, download time: {}. Folder: {}. Song list: {}",
            format_elapsed(started.elapsed().saturating_sub(scan_time)),
            folder.display(),
            catalog_path.display()
        );
    } else {
        println!(
            "Download finished with failures. Channel songs: {total_songs}, downloaded: {downloaded}, failed: {failed_downloads}, download time: {}. Incomplete .part files were kept for resume. Folder: {}. Song list: {}",
            format_elapsed(started.elapsed().saturating_sub(scan_time)),
            folder.display(),
            catalog_path.display()
        );
    }
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
    let show_icons = load_setting_bool("telegram.channel_icons", true);
    parse_private_channel(channel)
        .map(|(_, label)| {
            if show_icons {
                format!("🔒 {label}")
            } else {
                label
            }
        })
        .unwrap_or_else(|| {
            let label = format!("@{}", normalize_channel(channel));
            if show_icons {
                format!("🌐 {label}")
            } else {
                label
            }
        })
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
        text.push_str(&format!(
            "{}\t{}\t{name}\n",
            entry.message_id, entry.published_at
        ));
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
            let mut fields = line.splitn(3, '\t');
            let message_id = fields.next().context("missing message ID")?;
            let second = fields.next().with_context(|| {
                format!(
                    "invalid song list entry at {}:{}",
                    path.display(),
                    index + 1
                )
            })?;
            let (published_at, name) = match fields.next() {
                Some(name) => (
                    second.parse().with_context(|| {
                        format!("invalid published time at {}:{}", path.display(), index + 1)
                    })?,
                    name,
                ),
                None => (0, second),
            };
            Ok(TelegramCatalogEntry {
                channel: String::new(),
                message_id: message_id.parse().with_context(|| {
                    format!("invalid message ID at {}:{}", path.display(), index + 1)
                })?,
                published_at,
                name: name.to_string(),
            })
        })
        .collect()
}
