use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, ClearType};
use rodio::{OutputStream, Sink, Source};
use serde_json::Value;

use crate::audio_output::AudioOutput;
use crate::i18n::tr;
use crate::lyrics::{Lyrics, TimedLyricLine};
use crate::media_controls::{MediaCommand, MediaControls};
use crate::util::{
    DATA_DIR, LoopMode, MAX_VOLUME, PlayerExit, RawMode, TranscriptEditorResult,
    TranscriptSettingsEditor, bold_text, clear_screen, draw_panel, draw_transcript_player_panels,
    fit_text, format_duration, forward_track_index, insert_queue_next, load_setting_bool,
    load_setting_value, load_volume, manage_queue_with_adder, manage_transcript_settings,
    playback_bar_width, playback_controls_with_youtube, playback_key_bindings_hint,
    playback_progress_or_volume, player_title_row, previous_track_index, progress_bar, prompt,
    prompt_or_escape, restart_pass_order, save_setting_bool, save_setting_value, save_volume,
    select_menu, select_menu_from, set_shuffle_order, toggle_playback_key_bindings,
    wrap_playback_status,
};

const TOOLS_FILE: &str = ".music-terminal/youtube-tools.txt";
const SPONSORBLOCK_SETTINGS_FILE: &str = ".music-terminal/youtube-sponsorblock.txt";
const YOUTUBE_AUTH_METHOD: &str = "youtube.auth.method";
const YOUTUBE_AUTH_PROFILE: &str = "youtube.auth.thorium_profile";
const YOUTUBE_AUTH_BROWSER_PROFILE: &str = "youtube.auth.browser_profile";
const YOUTUBE_AUTH_CHROME_PROFILE: &str = "youtube.auth.chrome_profile";
const YOUTUBE_AUTH_VIVALDI_PROFILE: &str = "youtube.auth.vivaldi_profile";
const YOUTUBE_AUTH_ZEN_PROFILE: &str = "youtube.auth.zen_profile";
const YOUTUBE_AUTH_COOKIES_FILE: &str = "youtube.auth.cookies_file";
const AUDIO_CHANNELS: u16 = 2;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const SEEK_STEP: Duration = Duration::from_secs(10);
const RESUME_GAP: Duration = Duration::from_secs(5);
const TRACK_END_TOLERANCE: Duration = Duration::from_secs(5);
const MIN_PLAYBACK_RATE: f32 = 0.5;
const MAX_PLAYBACK_RATE: f32 = 2.0;
const PLAYBACK_RATE_STEP: f32 = 0.25;
const SPONSORBLOCK_API: &str = "https://sponsor.ajay.app/api/skipSegments";
const SPONSORBLOCK_CATEGORIES: [SponsorBlockCategory; 6] = [
    SponsorBlockCategory::new("sponsor", "Sponsor", true),
    SponsorBlockCategory::new("music_offtopic", "Non-music section", true),
    SponsorBlockCategory::new("interaction", "Interaction Reminder", false),
    SponsorBlockCategory::new("intro", "Intermission/Intro Animation", false),
    SponsorBlockCategory::new("outro", "Endcards/Credits (Outro)", false),
    SponsorBlockCategory::new("preview", "Preview/Recap", false),
];
const YT_DLP_DOWNLOAD_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YT_DLP_RELEASE_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const FFMPEG_DOWNLOAD_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const FFMPEG_VERSION_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.ver";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackInterruption {
    AudioOutput,
    Resume,
    Stream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SponsorBlockCategory {
    id: &'static str,
    label: &'static str,
    default_enabled: bool,
}

impl SponsorBlockCategory {
    const fn new(id: &'static str, label: &'static str, default_enabled: bool) -> Self {
        Self {
            id,
            label,
            default_enabled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SponsorBlockSettings {
    enabled: [bool; SPONSORBLOCK_CATEGORIES.len()],
}

impl Default for SponsorBlockSettings {
    fn default() -> Self {
        Self {
            enabled: SPONSORBLOCK_CATEGORIES.map(|category| category.default_enabled),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SponsorSegment {
    start: Duration,
    end: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct YouTubeTrack {
    video_id: String,
    title: String,
    webpage_url: String,
    duration: Option<Duration>,
    sponsor_segments: Option<Vec<SponsorSegment>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct YouTubeChapter {
    title: String,
    start: Duration,
    end: Option<Duration>,
}

enum ChapterLoadState {
    Loading,
    Ready(Vec<YouTubeChapter>),
    Failed(String),
}

struct ChaptersMenu {
    video_id: String,
    selected: usize,
    scroll: usize,
    seek_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptChoice {
    language: String,
    automatic: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum YouTubeAuthentication {
    None,
    Thorium { profile: String },
    Chrome { profile: String },
    Vivaldi { profile: String },
    Zen { profile: String },
    CookiesFile { path: PathBuf },
}

impl YouTubeAuthentication {
    fn from_values(method: Option<&str>, profile: Option<&str>, file: Option<&str>) -> Self {
        match method {
            Some("thorium") => Self::Thorium {
                profile: profile.unwrap_or("Default").to_string(),
            },
            Some("chrome") => Self::Chrome {
                profile: profile.unwrap_or("Default").to_string(),
            },
            Some("vivaldi") => Self::Vivaldi {
                profile: profile.unwrap_or("Default").to_string(),
            },
            Some("zen") => Self::Zen {
                profile: profile.unwrap_or("").to_string(),
            },
            Some("file") => Self::CookiesFile {
                path: PathBuf::from(file.unwrap_or("")),
            },
            _ => Self::None,
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Thorium { .. } => "thorium",
            Self::Chrome { .. } => "chrome",
            Self::Vivaldi { .. } => "vivaldi",
            Self::Zen { .. } => "zen",
            Self::CookiesFile { .. } => "file",
        }
    }
}

fn load_youtube_authentication() -> YouTubeAuthentication {
    let method = load_setting_value(YOUTUBE_AUTH_METHOD);
    let profile = if method.as_deref() == Some("thorium") {
        load_setting_value(YOUTUBE_AUTH_PROFILE)
    } else {
        method.as_deref().and_then(load_selected_browser_profile)
    };
    YouTubeAuthentication::from_values(
        method.as_deref(),
        profile.as_deref(),
        load_setting_value(YOUTUBE_AUTH_COOKIES_FILE).as_deref(),
    )
}

fn browser_profile_setting_key(method: &str) -> Option<&'static str> {
    match method {
        "chrome" => Some(YOUTUBE_AUTH_CHROME_PROFILE),
        "vivaldi" => Some(YOUTUBE_AUTH_VIVALDI_PROFILE),
        "zen" => Some(YOUTUBE_AUTH_ZEN_PROFILE),
        _ => None,
    }
}

fn load_selected_browser_profile(method: &str) -> Option<String> {
    load_setting_value(browser_profile_setting_key(method)?).or_else(|| {
        // Older versions used one shared profile setting for all three browsers.
        // Apply it only to the previously selected browser during migration.
        (load_setting_value(YOUTUBE_AUTH_METHOD).as_deref() == Some(method))
            .then(|| load_setting_value(YOUTUBE_AUTH_BROWSER_PROFILE))
            .flatten()
    })
}

fn thorium_user_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("Thorium").join("User Data"))
}

fn browser_user_data_dir(method: &str) -> Option<PathBuf> {
    match method {
        "thorium" => thorium_user_data_dir(),
        "chrome" => std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base)
                .join("Google")
                .join("Chrome")
                .join("User Data")
        }),
        "vivaldi" => std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Vivaldi").join("User Data")),
        "zen" => {
            std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("zen").join("Profiles"))
        }
        _ => None,
    }
}

fn browser_profiles(method: &str, root: &Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        bail!("{}", tr("msg.youtube_browser_not_found"));
    }
    if method != "zen" {
        return thorium_profiles(root);
    }
    let mut profiles = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir() && entry.path().join("prefs.js").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    profiles.sort();
    Ok(profiles)
}

fn browser_auth_arguments(method: &str, profile: &str, root: &Path) -> Result<Vec<OsString>> {
    let valid_name = if method == "zen" {
        !profile.is_empty() && !profile.contains(['/', '\\']) && profile != "." && profile != ".."
    } else {
        valid_thorium_profile(profile)
    };
    if !valid_name || !root.join(profile).is_dir() {
        bail!("{}", tr("msg.youtube_browser_profile_missing"));
    }
    let directory = root.join(profile);
    let has_cookies = if method == "zen" {
        directory.join("cookies.sqlite").is_file()
    } else {
        directory.join("Network").join("Cookies").is_file() || directory.join("Cookies").is_file()
    };
    if !has_cookies {
        bail!("{}", tr("msg.youtube_browser_cookies_missing"));
    }
    let mut argument = OsString::from(match method {
        "thorium" => "chromium:",
        "chrome" => "chrome:",
        "vivaldi" => "vivaldi:",
        "zen" => "firefox:",
        _ => bail!("unsupported browser"),
    });
    argument.push(directory.as_os_str());
    Ok(vec![OsString::from("--cookies-from-browser"), argument])
}

fn valid_thorium_profile(name: &str) -> bool {
    name == "Default"
        || name
            .strip_prefix("Profile ")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()))
}

fn thorium_profiles(root: &Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        bail!("{}", tr("msg.thorium_not_found"));
    }
    let mut profiles = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (valid_thorium_profile(&name) && entry.path().is_dir()).then_some(name)
        })
        .collect::<Vec<_>>();
    profiles.sort_by_key(|name| {
        (
            name != "Default",
            name.strip_prefix("Profile ")
                .unwrap_or("0")
                .parse::<u32>()
                .unwrap_or(u32::MAX),
        )
    });
    Ok(profiles)
}

fn validate_cookies_file(path: &Path) -> Result<()> {
    let file =
        fs::File::open(path).map_err(|_| anyhow::anyhow!("{}", tr("msg.cookies_file_missing")))?;
    // Only inspect the format header, never the cookie entries or values.
    let mut header = String::new();
    BufReader::new(file.take(256)).read_line(&mut header)?;
    if !matches!(
        header.trim(),
        "# Netscape HTTP Cookie File" | "# HTTP Cookie File"
    ) {
        bail!("{}", tr("msg.cookies_file_format"));
    }
    Ok(())
}

fn youtube_auth_arguments(
    auth: &YouTubeAuthentication,
    thorium_root: Option<&Path>,
) -> Result<Vec<OsString>> {
    match auth {
        YouTubeAuthentication::None => Ok(Vec::new()),
        YouTubeAuthentication::Thorium { profile } => {
            let root = thorium_root.context(tr("msg.thorium_not_found"))?;
            if !valid_thorium_profile(profile) || !root.join(profile).is_dir() {
                bail!("{}", tr("msg.thorium_profile_missing"));
            }
            let directory = root.join(profile);
            if !directory.join("Network").join("Cookies").is_file()
                && !directory.join("Cookies").is_file()
            {
                bail!("{}", tr("msg.thorium_cookies_missing"));
            }
            let mut argument = OsString::from("chromium:");
            argument.push(directory.as_os_str());
            Ok(vec![OsString::from("--cookies-from-browser"), argument])
        }
        YouTubeAuthentication::Chrome { profile } => browser_auth_arguments(
            "chrome",
            profile,
            &browser_user_data_dir("chrome").context(tr("msg.youtube_browser_not_found"))?,
        ),
        YouTubeAuthentication::Vivaldi { profile } => browser_auth_arguments(
            "vivaldi",
            profile,
            &browser_user_data_dir("vivaldi").context(tr("msg.youtube_browser_not_found"))?,
        ),
        YouTubeAuthentication::Zen { profile } => browser_auth_arguments(
            "zen",
            profile,
            &browser_user_data_dir("zen").context(tr("msg.youtube_browser_not_found"))?,
        ),
        YouTubeAuthentication::CookiesFile { path } => {
            validate_cookies_file(path)?;
            Ok(vec![
                OsString::from("--cookies"),
                path.as_os_str().to_os_string(),
            ])
        }
    }
}

fn append_youtube_auth_options(
    command: &mut Command,
    auth: &YouTubeAuthentication,
    thorium_root: Option<&Path>,
    use_android_client: bool,
) -> Result<()> {
    command.args(youtube_auth_arguments(auth, thorium_root)?);
    if use_android_client && *auth == YouTubeAuthentication::None {
        command.args(["--extractor-args", youtube_extractor_args()]);
    }
    Ok(())
}

fn configure_youtube_command(command: &mut Command, use_android_client: bool) -> Result<()> {
    append_youtube_auth_options(
        command,
        &load_youtube_authentication(),
        thorium_user_data_dir().as_deref(),
        use_android_client,
    )
}

fn youtube_request_error(operation: &str, stderr: &[u8]) -> String {
    youtube_request_error_for(&load_youtube_authentication(), operation, stderr)
}

fn youtube_request_error_for(
    auth: &YouTubeAuthentication,
    operation: &str,
    stderr: &[u8],
) -> String {
    if *auth == YouTubeAuthentication::None {
        format!("{operation}: {}", String::from_utf8_lossy(stderr).trim())
    } else {
        // yt-dlp output can contain private paths or request details. Only show safe hints.
        let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
        let explanation = if message.contains("decrypt")
            || message.contains("dpapi")
            || message.contains("app-bound")
            || message.contains("failed to load cookies")
        {
            tr("msg.youtube_cookie_decryption_error")
        } else if message.contains("locked")
            || message.contains("could not copy")
            || message.contains("permission denied")
        {
            tr("msg.youtube_cookie_locked_error")
        } else if message.contains("cookie") || message.contains("database") {
            tr("msg.youtube_cookie_error")
        } else if message.contains("sign in") || message.contains("login") {
            tr("msg.youtube_login_error")
        } else if message.contains("http error 403") || message.contains("http error 429") {
            tr("msg.youtube_http_error")
        } else {
            tr("msg.youtube_auth_request_failed")
        };
        format!("{operation}: {explanation}")
    }
}

fn youtube_auth_error_screen(error: &anyhow::Error) -> Result<()> {
    clear_screen()?;
    println!("{}: {error:#}", tr("msg.youtube_authentication"));
    prompt(tr("msg.press_enter_to_return_to_the_menu"))?;
    Ok(())
}

pub(crate) fn manage_youtube_authentication() -> Result<()> {
    let mut selected = 0;
    loop {
        let current = load_youtube_authentication();
        let mut items = vec![
            format!(
                "{}  {}",
                if current.method() == "none" {
                    "●"
                } else {
                    "○"
                },
                tr("msg.youtube_auth_none")
            ),
            format!(
                "{}  {}{}",
                if current.method() == "thorium" {
                    "●"
                } else {
                    "○"
                },
                tr("msg.youtube_auth_thorium"),
                match &current {
                    YouTubeAuthentication::Thorium { profile } => format!(" ({profile})"),
                    _ => String::new(),
                }
            ),
        ];
        for (method, label) in [
            ("chrome", tr("msg.youtube_auth_chrome")),
            ("vivaldi", tr("msg.youtube_auth_vivaldi")),
            ("zen", tr("msg.youtube_auth_zen")),
        ] {
            let profile = match &current {
                YouTubeAuthentication::Chrome { profile } if method == "chrome" => Some(profile),
                YouTubeAuthentication::Vivaldi { profile } if method == "vivaldi" => Some(profile),
                YouTubeAuthentication::Zen { profile } if method == "zen" => Some(profile),
                _ => None,
            };
            items.push(format!(
                "{}  {label}{}",
                if current.method() == method {
                    "●"
                } else {
                    "○"
                },
                profile.map_or(String::new(), |name| format!(" ({name})"))
            ));
        }
        items.push(format!(
            "{}  {}",
            if current.method() == "file" {
                "●"
            } else {
                "○"
            },
            tr("msg.youtube_auth_file")
        ));
        items.push(format!("←  {}", tr("msg.back")));
        match select_menu_from(tr("msg.youtube_authentication"), &items, selected)? {
            Some(0) => {
                save_setting_value(YOUTUBE_AUTH_METHOD, "none")?;
                selected = 0;
            }
            Some(1) => {
                selected = 1;
                let result = (|| -> Result<()> {
                    let root = thorium_user_data_dir().context(tr("msg.thorium_not_found"))?;
                    let profiles = thorium_profiles(&root)?;
                    if profiles.is_empty() {
                        bail!("{}", tr("msg.thorium_profile_missing"));
                    }
                    let selected_profile = match &current {
                        YouTubeAuthentication::Thorium { profile } => profile.as_str(),
                        _ => "Default",
                    };
                    let labels = profiles
                        .iter()
                        .map(|profile| {
                            format!(
                                "{}  {profile}",
                                if profile == selected_profile {
                                    "●"
                                } else {
                                    "○"
                                }
                            )
                        })
                        .chain(std::iter::once(format!("←  {}", tr("msg.back"))))
                        .collect::<Vec<_>>();
                    let start = profiles
                        .iter()
                        .position(|name| name == selected_profile)
                        .unwrap_or(0);
                    if let Some(index) =
                        select_menu_from(tr("msg.youtube_auth_thorium_profile"), &labels, start)?
                        && index < profiles.len()
                    {
                        // Validate the selected profile before persisting it.
                        youtube_auth_arguments(
                            &YouTubeAuthentication::Thorium {
                                profile: profiles[index].clone(),
                            },
                            Some(&root),
                        )?;
                        save_setting_value(YOUTUBE_AUTH_PROFILE, &profiles[index])?;
                        save_setting_value(YOUTUBE_AUTH_METHOD, "thorium")?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    youtube_auth_error_screen(&error)?;
                }
            }
            Some(choice @ 2..=4) => {
                selected = choice;
                let method = ["chrome", "vivaldi", "zen"][choice - 2];
                let result = (|| -> Result<()> {
                    let root = browser_user_data_dir(method)
                        .context(tr("msg.youtube_browser_not_found"))?;
                    let profiles = browser_profiles(method, &root)?;
                    if profiles.is_empty() {
                        bail!("{}", tr("msg.youtube_browser_profile_missing"));
                    }
                    let preferred = match &current {
                        YouTubeAuthentication::Chrome { profile } if method == "chrome" => {
                            Some(profile.clone())
                        }
                        YouTubeAuthentication::Vivaldi { profile } if method == "vivaldi" => {
                            Some(profile.clone())
                        }
                        YouTubeAuthentication::Zen { profile } if method == "zen" => {
                            Some(profile.clone())
                        }
                        _ => load_selected_browser_profile(method),
                    };
                    let preferred = preferred.as_deref().unwrap_or("Default");
                    let labels = profiles
                        .iter()
                        .map(|profile| {
                            format!(
                                "{}  {profile}",
                                if profile == preferred { "●" } else { "○" }
                            )
                        })
                        .chain(std::iter::once(format!("←  {}", tr("msg.back"))))
                        .collect::<Vec<_>>();
                    let start = profiles
                        .iter()
                        .position(|name| name == preferred)
                        .unwrap_or(0);
                    if let Some(index) =
                        select_menu_from(tr("msg.youtube_auth_select_profile"), &labels, start)?
                        && index < profiles.len()
                    {
                        browser_auth_arguments(method, &profiles[index], &root)?;
                        save_setting_value(
                            browser_profile_setting_key(method).expect("supported browser"),
                            &profiles[index],
                        )?;
                        save_setting_value(YOUTUBE_AUTH_METHOD, method)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    youtube_auth_error_screen(&error)?;
                }
            }
            Some(5) => {
                selected = 5;
                clear_screen()?;
                println!("{}", tr("msg.youtube_auth_file_hint"));
                if let Some(input) = prompt_or_escape(tr("msg.youtube_auth_file_prompt"))? {
                    let path = PathBuf::from(input.trim().trim_matches('"'));
                    if let Err(error) = validate_cookies_file(&path) {
                        youtube_auth_error_screen(&error)?;
                    } else {
                        save_setting_value(YOUTUBE_AUTH_COOKIES_FILE, &path.to_string_lossy())?;
                        save_setting_value(YOUTUBE_AUTH_METHOD, "file")?;
                    }
                }
            }
            Some(6) | None => return Ok(()),
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct YouTubeTools {
    pub(crate) yt_dlp: String,
    pub(crate) ffmpeg: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ToolUpdateVersions {
    yt_dlp: Option<String>,
    ffmpeg: Option<String>,
}

type SharedChild = Arc<Mutex<Child>>;

struct FfmpegPcmSource {
    child: SharedChild,
    stdout: ChildStdout,
}

struct YouTubePlayback {
    sink: Sink,
    ffmpeg: SharedChild,
    media_url: String,
    bitrate_kbps: Option<u32>,
    offset: Duration,
    rate: f32,
}

struct ResolvedAudioStream {
    url: String,
    bitrate_kbps: Option<u32>,
}

impl YouTubePlayback {
    fn stop(&self) {
        if let Ok(mut child) = self.ffmpeg.lock() {
            let _ = child.kill();
        }
        self.sink.stop();
    }

    fn elapsed(&self, duration: Option<Duration>) -> Duration {
        logical_elapsed(self.offset, self.sink.get_pos(), self.rate, duration)
    }
}

impl Deref for YouTubePlayback {
    type Target = Sink;

    fn deref(&self) -> &Self::Target {
        &self.sink
    }
}

impl Drop for YouTubePlayback {
    fn drop(&mut self) {
        if let Ok(mut child) = self.ffmpeg.lock() {
            let _ = child.kill();
        }
    }
}

impl Iterator for FfmpegPcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = [0; size_of::<f32>()];
        self.stdout.read_exact(&mut bytes).ok()?;
        Some(f32::from_le_bytes(bytes))
    }
}

impl Source for FfmpegPcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        AUDIO_CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        AUDIO_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Drop for FfmpegPcmSource {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn scaled_duration(duration: Duration, rate: f32) -> Duration {
    Duration::from_secs_f64(duration.as_secs_f64() * f64::from(rate))
}

fn logical_elapsed(
    offset: Duration,
    sink_elapsed: Duration,
    rate: f32,
    duration: Option<Duration>,
) -> Duration {
    let elapsed = offset.saturating_add(scaled_duration(sink_elapsed, rate));
    duration.map_or(elapsed, |total| elapsed.min(total))
}

fn resume_gap_detected(loop_gap: Duration) -> bool {
    loop_gap >= RESUME_GAP
}

fn playback_ended_early(elapsed: Duration, duration: Option<Duration>) -> bool {
    duration.is_some_and(|total| elapsed.saturating_add(TRACK_END_TOLERANCE) < total)
}

fn playback_interruption(
    audio_lost: bool,
    resumed: bool,
    stream_ended_early: bool,
) -> Option<PlaybackInterruption> {
    if audio_lost {
        Some(PlaybackInterruption::AudioOutput)
    } else if resumed {
        Some(PlaybackInterruption::Resume)
    } else if stream_ended_early {
        Some(PlaybackInterruption::Stream)
    } else {
        None
    }
}

fn recovery_message(interruption: PlaybackInterruption, stream_failures: usize) -> &'static str {
    match interruption {
        PlaybackInterruption::AudioOutput => "Audio output disconnected. Reconnecting...",
        PlaybackInterruption::Resume => "System resumed. Reconnecting playback...",
        PlaybackInterruption::Stream if stream_failures >= 3 => {
            "YouTube stream unavailable. Press p/Space to retry or choose another track."
        }
        PlaybackInterruption::Stream => "YouTube stream interrupted. Resolving a fresh stream...",
    }
}

fn seek_target(elapsed: Duration, duration: Option<Duration>, forward: bool) -> Duration {
    if forward {
        let target = elapsed.saturating_add(SEEK_STEP);
        duration.map_or(target, |total| target.min(total))
    } else {
        elapsed.saturating_sub(SEEK_STEP)
    }
}

fn stepped_rate(rate: f32, increase: bool) -> f32 {
    let next = if increase {
        rate + PLAYBACK_RATE_STEP
    } else {
        rate - PLAYBACK_RATE_STEP
    };
    next.clamp(MIN_PLAYBACK_RATE, MAX_PLAYBACK_RATE)
}

pub(crate) fn play_youtube() -> Result<PlayerExit> {
    loop {
        let items = vec![
            format!("▶  {}", tr("msg.play_youtube_url_or_playlist")),
            format!("◇  {}", tr("msg.youtube_authentication")),
            format!("✓  {}", tr("msg.sponsorblock_categories")),
            format!("≣  {}", tr("msg.transcript_settings")),
            format!("◇  {}", tr("msg.youtube_tools_and_updater")),
            format!("←  {}", tr("msg.back")),
        ];
        match select_menu(tr("msg.youtube_audio"), &items)? {
            Some(0) => {
                if play_youtube_url_or_playlist()? == PlayerExit::Quit {
                    return Ok(PlayerExit::Quit);
                }
            }
            Some(1) => manage_youtube_authentication()?,
            Some(2) => manage_sponsorblock_categories()?,
            Some(3) => manage_transcript_settings()?,
            Some(4) => manage_youtube_tools()?,
            Some(5) | None => return Ok(PlayerExit::Back),
            _ => unreachable!(),
        }
    }
}

pub(crate) fn play_youtube_url_or_playlist() -> Result<PlayerExit> {
    let tools = load_or_prompt_tools()?;
    let tracks = prompt_and_resolve_tracks(&tools)?;
    if tracks.is_empty() {
        return Ok(PlayerExit::Back);
    }
    play_youtube_tracks(tracks, &tools, &load_sponsorblock_settings())
}

fn parse_sponsorblock_settings(text: &str) -> SponsorBlockSettings {
    let mut settings = SponsorBlockSettings::default();
    for line in text.lines() {
        let Some((id, value)) = line.trim().split_once('=') else {
            continue;
        };
        let Some(index) = SPONSORBLOCK_CATEGORIES
            .iter()
            .position(|category| category.id == id.trim())
        else {
            continue;
        };
        match value.trim() {
            "auto_skip" => settings.enabled[index] = true,
            "no_skip" => settings.enabled[index] = false,
            _ => {}
        }
    }
    settings
}

fn serialize_sponsorblock_settings(settings: &SponsorBlockSettings) -> String {
    SPONSORBLOCK_CATEGORIES
        .iter()
        .zip(settings.enabled)
        .map(|(category, enabled)| {
            format!(
                "{}={}\n",
                category.id,
                if enabled { "auto_skip" } else { "no_skip" }
            )
        })
        .collect()
}

fn sponsorblock_categories_json(settings: &SponsorBlockSettings) -> String {
    let enabled: Vec<_> = SPONSORBLOCK_CATEGORIES
        .iter()
        .zip(settings.enabled)
        .filter_map(|(category, enabled)| enabled.then_some(category.id))
        .collect();
    serde_json::to_string(&enabled).expect("static SponsorBlock categories serialize")
}

fn load_sponsorblock_settings() -> SponsorBlockSettings {
    fs::read_to_string(SPONSORBLOCK_SETTINGS_FILE)
        .map(|text| parse_sponsorblock_settings(&text))
        .unwrap_or_default()
}

fn save_sponsorblock_settings(settings: &SponsorBlockSettings) -> Result<()> {
    fs::create_dir_all(DATA_DIR)?;
    fs::write(
        SPONSORBLOCK_SETTINGS_FILE,
        serialize_sponsorblock_settings(settings),
    )?;
    Ok(())
}

fn manage_sponsorblock_categories() -> Result<()> {
    let mut settings = load_sponsorblock_settings();
    let mut selected = 0;
    loop {
        let mut items: Vec<_> = SPONSORBLOCK_CATEGORIES
            .iter()
            .zip(settings.enabled)
            .map(|(category, enabled)| {
                format!(
                    "{:<9}  {}",
                    if enabled {
                        tr("msg.auto_skip")
                    } else {
                        tr("msg.no_skip")
                    },
                    category.label
                )
            })
            .collect();
        items.push(format!("←  {}", tr("msg.back")));

        match select_menu_from(tr("msg.sponsorblock_categories"), &items, selected)? {
            Some(index) if index < SPONSORBLOCK_CATEGORIES.len() => {
                settings.enabled[index] = !settings.enabled[index];
                save_sponsorblock_settings(&settings)?;
                selected = index;
            }
            Some(_) | None => return Ok(()),
        }
    }
}

fn load_or_prompt_tools() -> Result<YouTubeTools> {
    if let Ok(text) = fs::read_to_string(TOOLS_FILE)
        && let Some(tools) = parse_youtube_tools(&text)
    {
        let tools = repair_portable_tool_paths(tools)?;
        if validate_tool(&tools.yt_dlp, "--version").is_ok()
            && validate_tool(&tools.ffmpeg, "-version").is_ok()
        {
            return Ok(tools);
        }
    }

    let items = vec![
        format!("↓  {}", tr("msg.download_portable_yt_dlp_and_ffmpeg")),
        format!(
            "⌕  {}",
            tr("msg.use_existing_yt_dlp_and_ffmpeg_installations")
        ),
        format!("←  {}", tr("msg.back")),
    ];
    let tools = match select_menu(tr("msg.youtube_direct_stream_setup"), &items)? {
        Some(0) => download_portable_tools()?,
        Some(1) => prompt_existing_tools()?,
        Some(2) | None => bail!("YouTube setup cancelled"),
        _ => unreachable!(),
    };
    validate_tool(&tools.yt_dlp, "--version").context("yt-dlp validation failed")?;
    validate_tool(&tools.ffmpeg, "-version").context("FFmpeg validation failed")?;
    fs::create_dir_all(DATA_DIR)?;
    fs::write(TOOLS_FILE, serialize_youtube_tools(&tools))?;
    println!(
        "Saved yt-dlp and FFmpeg locations to {TOOLS_FILE}.\nyt-dlp: {}\nFFmpeg: {}",
        tools.yt_dlp, tools.ffmpeg
    );
    Ok(tools)
}

fn prompt_existing_tools() -> Result<YouTubeTools> {
    clear_screen()?;
    println!(
        "{}\n",
        tr("msg.enter_existing_executable_paths_command_names_on_path")
    );
    let tools = YouTubeTools {
        yt_dlp: normalize_executable_input(&prompt(tr("msg.yt_dlp_executable_path"))?),
        ffmpeg: normalize_executable_input(&prompt(tr("msg.ffmpeg_executable_path"))?),
    };
    if tools.yt_dlp.is_empty() || tools.ffmpeg.is_empty() {
        bail!("both yt-dlp and FFmpeg executable paths are required");
    }
    Ok(tools)
}

fn download_portable_tools() -> Result<YouTubeTools> {
    let executable = std::env::current_exe().context("failed to locate the player executable")?;
    let tools_dir = executable
        .parent()
        .context("player executable has no parent directory")?
        .join("tools");
    fs::create_dir_all(&tools_dir)
        .with_context(|| format!("failed to create {}", tools_dir.display()))?;
    let yt_dlp = tools_dir.join("yt-dlp.exe");
    let ffmpeg = tools_dir.join("ffmpeg.exe");
    let archive = tools_dir.join("ffmpeg.zip");
    let extracted = tools_dir.join("ffmpeg-extracted");

    clear_screen()?;
    println!("{}", tr("msg.downloading_portable_yt_dlp"));
    download_file(YT_DLP_DOWNLOAD_URL, &yt_dlp)?;
    println!(
        "{}",
        tr("msg.downloading_portable_ffmpeg_this_is_a_large_download")
    );
    download_file(FFMPEG_DOWNLOAD_URL, &archive)?;
    if extracted.exists() {
        fs::remove_dir_all(&extracted)?;
    }
    fs::create_dir_all(&extracted)?;
    println!("{}", tr("msg.extracting_ffmpeg"));
    let status = Command::new("tar.exe")
        .args(["-xf"])
        .arg(&archive)
        .arg("-C")
        .arg(&extracted)
        .status()
        .context("Windows tar.exe is required to extract portable FFmpeg")?;
    if !status.success() {
        bail!("failed to extract the FFmpeg archive");
    }
    let extracted_ffmpeg = find_file(&extracted, "ffmpeg.exe")?
        .context("the FFmpeg archive did not contain ffmpeg.exe")?;
    fs::copy(&extracted_ffmpeg, &ffmpeg)?;
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extracted);

    Ok(YouTubeTools {
        yt_dlp: yt_dlp.to_string_lossy().into_owned(),
        ffmpeg: ffmpeg.to_string_lossy().into_owned(),
    })
}

fn download_file(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl.exe")
        .args(["--fail", "--location", "--progress-bar", "--output"])
        .arg(destination)
        .arg(url)
        .status()
        .with_context(|| format!("failed to download {url} with Windows curl.exe"))?;
    if !status.success() {
        let _ = fs::remove_file(destination);
        bail!("download failed: {url}");
    }
    Ok(())
}

fn find_file(directory: &Path, name: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name)? {
                return Ok(Some(found));
            }
        } else if path
            .file_name()
            .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn normalize_executable_input(input: &str) -> String {
    input.trim().trim_matches('"').to_string()
}

fn validate_tool(executable: &str, version_argument: &str) -> Result<()> {
    let status = Command::new(executable)
        .arg(version_argument)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to start {executable}"))?;
    if !status.success() {
        bail!("{executable} {version_argument} exited unsuccessfully");
    }
    Ok(())
}

fn tool_version(executable: &str, version_argument: &str) -> String {
    Command::new(executable)
        .arg(version_argument)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

fn installed_version(version_output: &str, tool: &str) -> String {
    if version_output == "unavailable" {
        return version_output.to_string();
    }
    match tool {
        "FFmpeg" => version_output
            .strip_prefix("ffmpeg version ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or(version_output)
            .trim_end_matches("-essentials_build-www.gyan.dev")
            .to_string(),
        _ => version_output.trim().to_string(),
    }
}

fn version_numbers(version: &str) -> Vec<u64> {
    version
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn compare_versions(installed: &str, latest: &str) -> Ordering {
    let installed = version_numbers(installed);
    let latest = version_numbers(latest);
    let length = installed.len().max(latest.len());
    (0..length)
        .map(|index| {
            installed
                .get(index)
                .copied()
                .unwrap_or(0)
                .cmp(&latest.get(index).copied().unwrap_or(0))
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn update_status(installed: &str, latest: Option<&str>, checked: bool) -> String {
    let Some(latest) = latest else {
        return if checked {
            tr("msg.unable_to_check").to_string()
        } else {
            tr("msg.not_checked").to_string()
        };
    };
    if installed == "unavailable" {
        return format!(
            "{} {latest}; {}",
            tr("msg.latest"),
            tr("msg.installed_version_unavailable")
        );
    }
    match compare_versions(installed, latest) {
        Ordering::Less => format!("{}: {latest}", tr("msg.update_available")),
        Ordering::Equal => tr("msg.latest").to_string(),
        Ordering::Greater => format!("{} {latest}", tr("msg.newer_than_stable")),
    }
}

fn fetch_update_text(url: &str) -> Result<String> {
    let output = Command::new("curl.exe")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            "--user-agent",
            "Music-Terminal-Player",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to check {url}"))?;
    if !output.status.success() {
        bail!(
            "update check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_yt_dlp_release(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("tag_name")?
        .as_str()
        .map(str::to_string)
}

fn check_tool_updates() -> ToolUpdateVersions {
    ToolUpdateVersions {
        yt_dlp: fetch_update_text(YT_DLP_RELEASE_API)
            .ok()
            .and_then(|text| parse_yt_dlp_release(&text)),
        ffmpeg: fetch_update_text(FFMPEG_VERSION_URL)
            .ok()
            .filter(|version| !version.is_empty()),
    }
}

fn comparable_path(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn is_portable_yt_dlp(yt_dlp: &str, executable: &Path) -> bool {
    executable.parent().is_some_and(|directory| {
        comparable_path(Path::new(yt_dlp))
            == comparable_path(&directory.join("tools").join("yt-dlp.exe"))
    })
}

fn is_portable_ffmpeg(ffmpeg: &str, executable: &Path) -> bool {
    executable.parent().is_some_and(|directory| {
        comparable_path(Path::new(ffmpeg))
            == comparable_path(&directory.join("tools").join("ffmpeg.exe"))
    })
}

fn portable_tool_pair(tools: &YouTubeTools) -> bool {
    let yt_dlp = Path::new(&tools.yt_dlp);
    let ffmpeg = Path::new(&tools.ffmpeg);
    yt_dlp
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("yt-dlp.exe"))
        && ffmpeg
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("ffmpeg.exe"))
        && yt_dlp.parent().is_some_and(|directory| {
            directory
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("tools"))
                && ffmpeg.parent().is_some_and(|ffmpeg_directory| {
                    comparable_path(directory) == comparable_path(ffmpeg_directory)
                })
        })
}

fn relocated_portable_tools(tools: &YouTubeTools, executable: &Path) -> Option<YouTubeTools> {
    let directory = executable.parent()?;
    if !portable_tool_pair(tools) {
        return None;
    }
    let tools_dir = directory.join("tools");
    let relocated = YouTubeTools {
        yt_dlp: tools_dir.join("yt-dlp.exe").to_string_lossy().into_owned(),
        ffmpeg: tools_dir.join("ffmpeg.exe").to_string_lossy().into_owned(),
    };
    (comparable_path(Path::new(&tools.yt_dlp)) != comparable_path(Path::new(&relocated.yt_dlp))
        && (Path::new(&relocated.yt_dlp).exists() || Path::new(&relocated.ffmpeg).exists()))
    .then_some(relocated)
}

fn repair_portable_tool_paths(tools: YouTubeTools) -> Result<YouTubeTools> {
    let executable = std::env::current_exe().context("failed to locate the player executable")?;
    let Some(relocated) = relocated_portable_tools(&tools, &executable) else {
        return Ok(tools);
    };
    fs::create_dir_all(DATA_DIR)?;
    fs::write(TOOLS_FILE, serialize_youtube_tools(&relocated))?;
    Ok(relocated)
}

fn portable_yt_dlp_update_paths(yt_dlp: &Path) -> (PathBuf, PathBuf) {
    (
        yt_dlp.with_extension("exe.download"),
        yt_dlp.with_extension("exe.backup"),
    )
}

fn portable_ffmpeg_update_paths(ffmpeg: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let directory = ffmpeg.parent().unwrap_or_else(|| Path::new("."));
    (
        directory.join("ffmpeg.zip"),
        directory.join("ffmpeg-extracted"),
        ffmpeg.with_extension("exe.download"),
        ffmpeg.with_extension("exe.backup"),
    )
}

fn install_staged_tool(
    target: &Path,
    download: &Path,
    backup: &Path,
    tool_name: &str,
) -> Result<()> {
    let had_existing = target.exists();
    if had_existing {
        fs::rename(target, backup)
            .with_context(|| format!("failed to back up portable {tool_name}"))?;
    }
    if let Err(error) = fs::rename(download, target) {
        if had_existing {
            let _ = fs::rename(backup, target);
        }
        let _ = fs::remove_file(download);
        return Err(error)
            .with_context(|| format!("failed to install portable {tool_name} update"));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn update_portable_yt_dlp(yt_dlp: &Path) -> Result<()> {
    let (download, backup) = portable_yt_dlp_update_paths(yt_dlp);
    let _ = fs::remove_file(&download);
    let _ = fs::remove_file(&backup);
    download_file(YT_DLP_DOWNLOAD_URL, &download)?;

    let downloaded = download.to_string_lossy();
    if let Err(error) = validate_tool(downloaded.as_ref(), "--version") {
        let _ = fs::remove_file(&download);
        return Err(error).context("downloaded yt-dlp failed validation");
    }

    install_staged_tool(yt_dlp, &download, &backup, "yt-dlp")
}

fn update_portable_ffmpeg(ffmpeg: &Path) -> Result<()> {
    let (archive, extracted, download, backup) = portable_ffmpeg_update_paths(ffmpeg);
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extracted);
    let _ = fs::remove_file(&download);
    let _ = fs::remove_file(&backup);

    download_file(FFMPEG_DOWNLOAD_URL, &archive)?;
    fs::create_dir_all(&extracted)?;
    let status = Command::new("tar.exe")
        .args(["-xf"])
        .arg(&archive)
        .arg("-C")
        .arg(&extracted)
        .status()
        .context("Windows tar.exe is required to extract portable FFmpeg")?;
    if !status.success() {
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_dir_all(&extracted);
        bail!("failed to extract the FFmpeg archive");
    }

    let extracted_ffmpeg = find_file(&extracted, "ffmpeg.exe")?
        .context("the FFmpeg archive did not contain ffmpeg.exe")?;
    fs::copy(extracted_ffmpeg, &download).context("failed to stage portable FFmpeg update")?;
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extracted);

    let downloaded = download.to_string_lossy();
    if let Err(error) = validate_tool(downloaded.as_ref(), "-version") {
        let _ = fs::remove_file(&download);
        return Err(error).context("downloaded FFmpeg failed validation");
    }

    install_staged_tool(ffmpeg, &download, &backup, "FFmpeg")
}

pub(crate) fn youtube_tool_menu_items() -> Vec<String> {
    vec![
        format!("⌕  {}", tr("msg.check_for_tool_updates")),
        format!("✎  {}", tr("msg.use_your_own_yt_dlp_and_ffmpeg")),
        format!("↓  {}", tr("msg.update_portable_yt_dlp_stable")),
        format!("↓  {}", tr("msg.update_portable_ffmpeg_gyan_essentials")),
        format!("↓  {}", tr("msg.update_both_portable_tools")),
        format!("←  {}", tr("msg.back")),
    ]
}

fn manage_youtube_tools() -> Result<()> {
    let mut tools = match fs::read_to_string(TOOLS_FILE)
        .ok()
        .and_then(|text| parse_youtube_tools(&text))
        .map(repair_portable_tool_paths)
        .transpose()?
    {
        Some(tools) => tools,
        None => load_or_prompt_tools()?,
    };
    let mut updates: Option<ToolUpdateVersions> = None;
    loop {
        let executable =
            std::env::current_exe().context("failed to locate the player executable")?;
        let portable_yt_dlp = is_portable_yt_dlp(&tools.yt_dlp, &executable);
        let portable_ffmpeg = is_portable_ffmpeg(&tools.ffmpeg, &executable);
        let yt_dlp_version = installed_version(&tool_version(&tools.yt_dlp, "--version"), "yt-dlp");
        let ffmpeg_version = installed_version(&tool_version(&tools.ffmpeg, "-version"), "FFmpeg");
        let checked = updates.is_some();
        let title = format!(
            "{}\nyt-dlp: {} ({})\nFFmpeg: {} ({})\n{}: {}",
            tr("msg.youtube_tools"),
            yt_dlp_version,
            update_status(
                &yt_dlp_version,
                updates
                    .as_ref()
                    .and_then(|versions| versions.yt_dlp.as_deref()),
                checked,
            ),
            ffmpeg_version,
            update_status(
                &ffmpeg_version,
                updates
                    .as_ref()
                    .and_then(|versions| versions.ffmpeg.as_deref()),
                checked,
            ),
            tr("msg.updater"),
            if portable_yt_dlp || portable_ffmpeg {
                tr("msg.portable_stable_channels")
            } else {
                tr("msg.disabled_for_external_path_installations")
            }
        );
        let items = youtube_tool_menu_items();
        match select_menu(&title, &items)? {
            Some(0) => {
                clear_screen()?;
                println!("{}", tr("msg.checking_stable_yt_dlp_and_ffmpeg_versions"));
                updates = Some(check_tool_updates());
            }
            Some(1) => {
                let replacement = prompt_existing_tools()?;
                validate_tool(&replacement.yt_dlp, "--version")
                    .context("yt-dlp validation failed")?;
                validate_tool(&replacement.ffmpeg, "-version")
                    .context("FFmpeg validation failed")?;
                fs::create_dir_all(DATA_DIR)?;
                fs::write(TOOLS_FILE, serialize_youtube_tools(&replacement))?;
                tools = replacement;
                updates = None;
            }
            Some(2) if portable_yt_dlp => {
                clear_screen()?;
                println!(
                    "{}",
                    tr("msg.downloading_the_latest_stable_portable_yt_dlp")
                );
                update_portable_yt_dlp(Path::new(&tools.yt_dlp))?;
                updates = Some(check_tool_updates());
                println!("{}", tr("msg.yt_dlp_updated_successfully"));
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(2) => {
                clear_screen()?;
                println!(
                    "{}\n{}: {}",
                    tr("msg.this_updater_only_manages_the_application_relative_tools"),
                    tr("msg.configured_yt_dlp"),
                    tools.yt_dlp
                );
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(3) if portable_ffmpeg => {
                clear_screen()?;
                println!(
                    "{}",
                    tr("msg.downloading_the_latest_portable_ffmpeg_essentials_build")
                );
                update_portable_ffmpeg(Path::new(&tools.ffmpeg))?;
                updates = Some(check_tool_updates());
                println!("{}", tr("msg.ffmpeg_updated_successfully"));
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(3) => {
                clear_screen()?;
                println!(
                    "{}\n{}: {}",
                    tr("msg.this_updater_only_manages_the_application_relative_tools_2"),
                    tr("msg.configured_ffmpeg"),
                    tools.ffmpeg
                );
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(4) if portable_yt_dlp && portable_ffmpeg => {
                clear_screen()?;
                println!(
                    "{}",
                    tr("msg.downloading_the_latest_stable_portable_yt_dlp")
                );
                update_portable_yt_dlp(Path::new(&tools.yt_dlp))?;
                println!(
                    "{}",
                    tr("msg.downloading_the_latest_portable_ffmpeg_essentials_build")
                );
                update_portable_ffmpeg(Path::new(&tools.ffmpeg))?;
                updates = Some(check_tool_updates());
                println!("{}", tr("msg.yt_dlp_and_ffmpeg_updated_successfully"));
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(4) => {
                clear_screen()?;
                println!(
                    "{}",
                    tr("msg.updating_both_requires_application_relative_tools_yt_dlp")
                );
                prompt(tr("msg.press_enter_to_go_back"))?;
            }
            Some(5) | None => return Ok(()),
            _ => unreachable!(),
        }
    }
}

pub(crate) fn parse_youtube_tools(text: &str) -> Option<YouTubeTools> {
    let mut yt_dlp = None;
    let mut ffmpeg = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("yt_dlp=") {
            yt_dlp = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("ffmpeg=") {
            ffmpeg = Some(value.to_string());
        }
    }
    Some(YouTubeTools {
        yt_dlp: yt_dlp.filter(|value| !value.is_empty())?,
        ffmpeg: ffmpeg.filter(|value| !value.is_empty())?,
    })
}

pub(crate) fn serialize_youtube_tools(tools: &YouTubeTools) -> String {
    format!("yt_dlp={}\nffmpeg={}\n", tools.yt_dlp, tools.ffmpeg)
}

fn prompt_and_resolve_tracks(tools: &YouTubeTools) -> Result<Vec<YouTubeTrack>> {
    let mut error = None::<(String, bool)>;
    loop {
        clear_screen()?;
        println!(
            "{}",
            tr("msg.youtube_audio_tutor_paste_one_url_multiple_space")
        );
        println!("{}", tr("msg.esc_back"));
        if let Some((error, invalid_url)) = &error {
            let label = if *invalid_url {
                tr("msg.invalid_youtube_url")
            } else {
                tr("msg.youtube_request_failed")
            };
            println!("\n{label}: {error}");
        }

        let Some(input) = prompt_or_escape(tr("msg.url_s"))? else {
            return Ok(Vec::new());
        };
        if input.trim().is_empty() {
            return Ok(Vec::new());
        }

        let urls = match parse_youtube_urls(&input) {
            Ok(urls) => urls,
            Err(parse_error) => {
                error = Some((format!("{parse_error:#}"), true));
                continue;
            }
        };

        match resolve_tracks(tools, &urls) {
            Ok(tracks) => return Ok(tracks),
            Err(resolve_error) => error = Some((format!("{resolve_error:#}"), false)),
        }
    }
}

pub(crate) fn parse_youtube_urls(input: &str) -> Result<Vec<String>> {
    input
        .split_whitespace()
        .map(|value| {
            let lower = value.to_ascii_lowercase();
            let valid_scheme = lower.starts_with("https://") || lower.starts_with("http://");
            let host = lower
                .split_once("://")
                .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("");
            let valid_host = matches!(
                host,
                "youtube.com"
                    | "www.youtube.com"
                    | "m.youtube.com"
                    | "music.youtube.com"
                    | "youtu.be"
            );
            if valid_scheme && valid_host {
                Ok(value.to_string())
            } else {
                bail!("unsupported YouTube URL: {value}")
            }
        })
        .collect()
}

fn resolve_tracks(tools: &YouTubeTools, urls: &[String]) -> Result<Vec<YouTubeTrack>> {
    clear_screen()?;
    println!(
        "{}",
        tr("msg.resolving_youtube_video_and_playlist_metadata")
    );
    let mut command = Command::new(&tools.yt_dlp);
    command.args([
        "--no-warnings",
        "--ignore-errors",
        "--yes-playlist",
        "--flat-playlist",
        "--print",
        "%(id)s\t%(webpage_url)s\t%(title)S\t%(duration)s",
    ]);
    configure_youtube_command(&mut command, false)?;
    let output = command
        .args(urls)
        .stdin(Stdio::null())
        .output()
        .context("failed to run yt-dlp")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            youtube_request_error("yt-dlp failed to resolve YouTube input", error.as_bytes())
        );
    }
    let tracks: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_youtube_track_line)
        .collect();
    if tracks.is_empty() {
        bail!("yt-dlp returned no playable YouTube tracks");
    }
    Ok(tracks)
}

fn parse_youtube_track_line(line: &str) -> Option<YouTubeTrack> {
    let mut fields = line.splitn(4, '\t');
    let video_id = fields.next()?.trim();
    let webpage_url = fields.next()?.trim();
    let title = fields.next()?.trim();
    let duration = fields
        .next()?
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|seconds| {
            seconds
                .is_finite()
                .then(|| Duration::from_secs_f64(seconds.max(0.0)))
        });
    if video_id.is_empty() || webpage_url.is_empty() {
        return None;
    }
    Some(YouTubeTrack {
        video_id: video_id.to_string(),
        title: if title.is_empty() {
            format!("YouTube video {video_id}")
        } else {
            title.to_string()
        },
        webpage_url: webpage_url.to_string(),
        duration,
        sponsor_segments: None,
    })
}

fn transcript_preference() -> String {
    load_setting_value("transcript.language").unwrap_or_else(|| "auto".to_string())
}

fn transcript_memory_key(track: &YouTubeTrack) -> String {
    format!("{}|{}", track.video_id, transcript_preference())
}

fn transcript_cache_path(track: &YouTubeTrack) -> PathBuf {
    let preference = transcript_preference();
    Path::new(DATA_DIR)
        .join("youtube-transcripts")
        .join(format!("{}-{preference}.vtt", track.video_id))
}

fn available_caption_languages(metadata: &Value, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .and_then(Value::as_object)
        .map(|captions| {
            captions
                .keys()
                .filter(|language| language.as_str() != "live_chat")
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn matching_caption_language(languages: &[String], wanted: &str) -> Option<String> {
    if wanted.is_empty() {
        return None;
    }
    languages
        .iter()
        .find(|language| language.eq_ignore_ascii_case(wanted))
        .or_else(|| {
            languages.iter().find(|language| {
                language
                    .strip_prefix(wanted)
                    .is_some_and(|suffix| suffix.starts_with('-'))
            })
        })
        .cloned()
}

fn original_auto_caption_language(languages: &[String]) -> Option<String> {
    languages
        .iter()
        .find(|language| language.ends_with("-orig"))
        .cloned()
}

fn choose_transcript(metadata: &Value, preference: &str) -> Option<TranscriptChoice> {
    let manual = available_caption_languages(metadata, "subtitles");
    let automatic = available_caption_languages(metadata, "automatic_captions");
    let video_language = metadata
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let manual_choice = |language: String| TranscriptChoice {
        language,
        automatic: false,
    };
    let automatic_choice = |language: String| TranscriptChoice {
        language,
        automatic: true,
    };

    if matches!(preference, "en" | "vi" | "ja") {
        return matching_caption_language(&manual, preference)
            .map(manual_choice)
            .or_else(|| matching_caption_language(&automatic, preference).map(automatic_choice))
            .or_else(|| original_auto_caption_language(&automatic).map(automatic_choice))
            .or_else(|| manual.first().cloned().map(manual_choice))
            .or_else(|| automatic.first().cloned().map(automatic_choice));
    }

    if preference == "original" {
        return matching_caption_language(&manual, video_language)
            .map(manual_choice)
            .or_else(|| original_auto_caption_language(&automatic).map(automatic_choice))
            .or_else(|| matching_caption_language(&automatic, video_language).map(automatic_choice))
            .or_else(|| manual.first().cloned().map(manual_choice))
            .or_else(|| automatic.first().cloned().map(automatic_choice));
    }

    matching_caption_language(&manual, video_language)
        .map(manual_choice)
        .or_else(|| manual.first().cloned().map(manual_choice))
        .or_else(|| original_auto_caption_language(&automatic).map(automatic_choice))
        .or_else(|| matching_caption_language(&automatic, video_language).map(automatic_choice))
        .or_else(|| matching_caption_language(&automatic, "en").map(automatic_choice))
        .or_else(|| automatic.first().cloned().map(automatic_choice))
}

fn parse_youtube_chapters(metadata: &Value) -> Vec<YouTubeChapter> {
    let mut chapters: Vec<_> = metadata
        .get("chapters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let title = entry.get("title")?.as_str()?.trim();
            let start = entry.get("start_time")?.as_f64()?;
            if title.is_empty() || !start.is_finite() || !(0.0..=1.0e9).contains(&start) {
                return None;
            }
            let end = match entry.get("end_time") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let end = value.as_f64()?;
                    if !end.is_finite() || end <= start || end > 1.0e9 {
                        return None;
                    }
                    Some(Duration::from_secs_f64(end))
                }
            };
            Some(YouTubeChapter {
                title: title.to_string(),
                start: Duration::from_secs_f64(start),
                end,
            })
        })
        .collect();
    chapters.sort_by_key(|chapter| chapter.start);
    chapters.dedup_by_key(|chapter| chapter.start);
    chapters
}

fn active_chapter(chapters: &[YouTubeChapter], position: Duration) -> Option<usize> {
    chapters.iter().rposition(|chapter| {
        chapter.start <= position && chapter.end.is_none_or(|end| position < end)
    })
}

fn resolve_chapters(tools: &YouTubeTools, track: &YouTubeTrack) -> Result<Vec<YouTubeChapter>> {
    let mut command = Command::new(&tools.yt_dlp);
    command.args([
        "--no-warnings",
        "--no-playlist",
        "--skip-download",
        "--dump-single-json",
    ]);
    configure_youtube_command(&mut command, true)?;
    let output = command
        .arg(&track.webpage_url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to retrieve chapters for {}", track.title))?;
    if !output.status.success() {
        bail!(
            "{}",
            youtube_request_error("yt-dlp chapter metadata lookup failed", &output.stderr)
        );
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .context("yt-dlp returned invalid chapter metadata JSON")?;
    Ok(parse_youtube_chapters(&metadata))
}

fn request_chapters(
    cache: &mut HashMap<String, ChapterLoadState>,
    sender: &Sender<(String, Result<Vec<YouTubeChapter>, String>)>,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
) {
    let id = track.video_id.clone();
    cache.insert(id.clone(), ChapterLoadState::Loading);
    let sender = sender.clone();
    let tools = tools.clone();
    let track = track.clone();
    thread::spawn(move || {
        let result = resolve_chapters(&tools, &track).map_err(|error| error.to_string());
        let _ = sender.send((id, result));
    });
}

fn chapter_page_size(height: u16) -> usize {
    usize::from(height).saturating_sub(6).max(1)
}

fn chapter_menu_rows(
    menu: &mut ChaptersMenu,
    state: &ChapterLoadState,
    elapsed: Duration,
    columns: u16,
    height: u16,
) -> Vec<String> {
    let content_width = usize::from(columns).saturating_sub(5).min(60);
    let available_rows = chapter_page_size(height);
    let mut rows = Vec::new();
    match state {
        ChapterLoadState::Loading => rows.push(tr("msg.chapters_loading").to_string()),
        ChapterLoadState::Failed(error) => {
            rows.push(tr("msg.chapters_failed").to_string());
            if available_rows >= 3 {
                rows.push(fit_text(error, content_width));
            }
            rows.push(tr("msg.chapters_retry").to_string());
        }
        ChapterLoadState::Ready(chapters) if chapters.is_empty() => {
            rows.push(tr("msg.no_chapters_available").to_string());
        }
        ChapterLoadState::Ready(chapters) => {
            let page_size = available_rows
                .saturating_sub(usize::from(menu.seek_error.is_some()))
                .max(1);
            menu.selected = menu.selected.min(chapters.len() - 1);
            menu.scroll = menu.scroll.min(chapters.len().saturating_sub(page_size));
            if menu.selected < menu.scroll {
                menu.scroll = menu.selected;
            } else if menu.selected >= menu.scroll + page_size {
                menu.scroll = menu.selected + 1 - page_size;
            }
            let active = active_chapter(chapters, elapsed);
            for (index, chapter) in chapters
                .iter()
                .enumerate()
                .skip(menu.scroll)
                .take(page_size)
            {
                let prefix = format!(
                    "{} {}  ",
                    if index == menu.selected { '>' } else { ' ' },
                    format_duration(chapter.start)
                );
                let suffix = if Some(index) == active { " ♪" } else { "" };
                let remaining =
                    content_width.saturating_sub(prefix.chars().count() + suffix.chars().count());
                rows.push(format!(
                    "{prefix}{}{suffix}",
                    fit_text(&chapter.title, remaining)
                ));
            }
            if let Some(error) = &menu.seek_error {
                rows.push(fit_text(error, content_width));
            }
        }
    }
    let hint = if content_width < 30 {
        tr("msg.chapters_keys_short")
    } else if content_width < 60 {
        tr("msg.chapters_keys_medium")
    } else {
        tr("msg.chapters_keys")
    };
    // Keep a full row for the menu footer, including on short terminals.
    rows.truncate(available_rows);
    if usize::from(height) >= 7 {
        rows.push(fit_text(&hint, content_width));
    }
    rows
}

fn draw_chapters_menu(
    stdout: &mut io::Stdout,
    menu: &mut ChaptersMenu,
    state: &ChapterLoadState,
    elapsed: Duration,
) -> Result<()> {
    let (columns, height) = terminal::size().unwrap_or((80, 24));
    if columns < 18 || height < 8 {
        return crate::util::draw_frame(stdout, "Resize terminal");
    }
    let title = if columns < 27 {
        tr("msg.chapters")
    } else {
        tr("msg.youtube_chapters")
    };
    let rows = chapter_menu_rows(menu, state, elapsed, columns, height);
    draw_panel(stdout, &title, &rows)
}

fn resolve_transcript(tools: &YouTubeTools, track: &YouTubeTrack) -> Result<Option<Lyrics>> {
    let cache_path = transcript_cache_path(track);
    if let Ok(text) = fs::read_to_string(&cache_path)
        && let Some(transcript) = parse_webvtt_transcript(&text)
    {
        return Ok(Some(transcript));
    }

    let mut metadata_command = Command::new(&tools.yt_dlp);
    metadata_command.args([
        "--no-warnings",
        "--no-playlist",
        "--skip-download",
        "--dump-single-json",
    ]);
    configure_youtube_command(&mut metadata_command, true)?;
    let metadata_output = metadata_command
        .arg(&track.webpage_url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to inspect captions for {}", track.title))?;
    if !metadata_output.status.success() {
        return Ok(None);
    }
    let metadata: Value = serde_json::from_slice(&metadata_output.stdout)
        .context("yt-dlp returned invalid YouTube metadata JSON")?;
    let preference = transcript_preference();
    let Some(choice) = choose_transcript(&metadata, &preference) else {
        return Ok(None);
    };

    let cache_dir = cache_path
        .parent()
        .context("transcript cache path has no parent")?;
    fs::create_dir_all(cache_dir)?;
    let temp_stem = format!(".{}-caption-{}", track.video_id, std::process::id());
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&temp_stem) {
            let _ = fs::remove_file(entry.path());
        }
    }
    let output_template = cache_dir.join(&temp_stem);
    let mut command = Command::new(&tools.yt_dlp);
    command.args([
        "--no-warnings",
        "--no-playlist",
        "--skip-download",
        "--sub-format",
        "vtt",
        "--sub-langs",
    ]);
    configure_youtube_command(&mut command, true)?;
    command.arg(&choice.language);
    if choice.automatic {
        command.arg("--write-auto-subs");
    } else {
        command.arg("--write-subs");
    }
    let output = command
        .arg("-o")
        .arg(&output_template)
        .arg(&track.webpage_url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to download transcript for {}", track.title))?;
    if !output.status.success() {
        return Ok(None);
    }

    let mut downloaded = None;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&temp_stem)
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("vtt"))
        {
            downloaded = Some(entry.path());
            break;
        }
    }
    let Some(downloaded) = downloaded else {
        return Ok(None);
    };
    let text = fs::read_to_string(&downloaded)?;
    fs::write(&cache_path, &text)?;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&temp_stem) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(parse_webvtt_transcript(&text))
}

fn ensure_transcript(
    cache: &mut HashMap<String, Option<Lyrics>>,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
) {
    let key = transcript_memory_key(track);
    if cache.contains_key(&key) {
        return;
    }
    let transcript = resolve_transcript(tools, track).ok().flatten();
    cache.insert(key, transcript);
}

fn cached_transcript<'a>(
    cache: &'a HashMap<String, Option<Lyrics>>,
    track: &YouTubeTrack,
) -> Option<&'a Lyrics> {
    cache
        .get(&transcript_memory_key(track))
        .and_then(Option::as_ref)
}

fn parse_webvtt_timestamp(value: &str) -> Option<Duration> {
    let parts = value.trim().split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (
            0u64,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        _ => return None,
    };
    let total = hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds;
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

fn clean_webvtt_text(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => plain.push(character),
            _ => {}
        }
    }
    let plain = plain
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_webvtt_transcript(text: &str) -> Option<Lyrics> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().peekable();
    let mut transcript = Vec::new();
    while let Some(line) = lines.next() {
        let Some((start, _)) = line.split_once("-->") else {
            continue;
        };
        let Some(at) = parse_webvtt_timestamp(start) else {
            continue;
        };
        let mut cue_lines = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() {
                lines.next();
                break;
            }
            cue_lines.push(lines.next().unwrap_or_default().trim());
        }
        let cue = clean_webvtt_text(&cue_lines.join(" "));
        if cue.is_empty()
            || transcript
                .last()
                .is_some_and(|previous: &TimedLyricLine| previous.text == cue)
        {
            continue;
        }
        transcript.push(TimedLyricLine { at, text: cue });
    }
    (!transcript.is_empty()).then_some(Lyrics::Timed(transcript))
}

fn youtube_stream_format() -> &'static str {
    // Prefer audio-only Opus/AAC for quality, then fall back to progressive MP4 for compatibility.
    // Keep itag 18 available because its front-loaded index has historically made seeking reliable.
    "251/140/18/bestaudio/best"
}

fn youtube_extractor_args() -> &'static str {
    // ponytail: Android currently avoids rejected ANDROID_VR stream signatures. Add client fallback
    // probing if YouTube starts varying working clients by video or account.
    "youtube:player_client=android"
}

fn parse_resolved_audio_stream(line: &str) -> Option<ResolvedAudioStream> {
    let mut fields = line.trim().splitn(3, '\t');
    let format_id = fields.next()?;
    let url = fields.next()?;
    let abr = fields.next()?;
    if url.is_empty() {
        return None;
    }
    let bitrate_kbps = abr
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.round() as u32)
        .or_else(|| match format_id {
            "251" => Some(160),
            "140" => Some(128),
            "18" => Some(96),
            _ => None,
        });
    Some(ResolvedAudioStream {
        url: url.to_string(),
        bitrate_kbps,
    })
}

fn resolve_audio_url(tools: &YouTubeTools, track: &YouTubeTrack) -> Result<ResolvedAudioStream> {
    let mut command = Command::new(&tools.yt_dlp);
    command.args([
        "--no-warnings",
        "--no-playlist",
        "--format",
        youtube_stream_format(),
        "--print",
        "%(format_id)s\t%(url)s\t%(abr)s",
    ]);
    configure_youtube_command(&mut command, true)?;
    let output = command
        .arg(&track.webpage_url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to resolve {}", track.title))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            youtube_request_error("yt-dlp failed to resolve audio stream", error.as_bytes())
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(parse_resolved_audio_stream)
        .context("yt-dlp returned no direct audio stream URL")
}

fn spawn_ffmpeg_source(
    ffmpeg: &str,
    media_url: &str,
    offset: Duration,
    rate: f32,
) -> Result<(FfmpegPcmSource, SharedChild)> {
    let mut command = Command::new(ffmpeg);
    command.args([
        "-nostdin",
        "-loglevel",
        "error",
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_delay_max",
        "5",
        "-seekable",
        "1",
    ]);
    if !offset.is_zero() {
        command
            .arg("-ss")
            .arg(format!("{:.3}", offset.as_secs_f64()));
    }
    let mut child = command
        .arg("-i")
        .arg(media_url)
        .args(["-vn", "-filter:a"])
        .arg(format!("atempo={rate:.2}"))
        .args([
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ac",
            "2",
            "-ar",
            "48000",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start FFmpeg")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to open FFmpeg audio pipe")?;
    let child = Arc::new(Mutex::new(child));
    Ok((
        FfmpegPcmSource {
            child: Arc::clone(&child),
            stdout,
        },
        child,
    ))
}

fn playback_from_url(
    stream: &OutputStream,
    tools: &YouTubeTools,
    media_url: String,
    bitrate_kbps: Option<u32>,
    volume: f32,
    offset: Duration,
    rate: f32,
    paused: bool,
) -> Result<YouTubePlayback> {
    let (source, ffmpeg) = spawn_ffmpeg_source(&tools.ffmpeg, &media_url, offset, rate)?;
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    sink.append(source);
    if paused {
        sink.pause();
    }
    Ok(YouTubePlayback {
        sink,
        ffmpeg,
        media_url,
        bitrate_kbps,
        offset,
        rate,
    })
}

fn start_track(
    stream: &OutputStream,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
    volume: f32,
    rate: f32,
) -> Result<YouTubePlayback> {
    let resolved = resolve_audio_url(tools, track)?;
    playback_from_url(
        stream,
        tools,
        resolved.url,
        resolved.bitrate_kbps,
        volume,
        Duration::ZERO,
        rate,
        false,
    )
}

fn replace_track(
    playback: &mut YouTubePlayback,
    stream: &OutputStream,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
    volume: f32,
    rate: f32,
) -> Result<()> {
    let replacement = start_track(stream, tools, track, volume, rate)?;
    playback.stop();
    *playback = replacement;
    Ok(())
}

fn restart_playback(
    playback: &mut YouTubePlayback,
    stream: &OutputStream,
    tools: &YouTubeTools,
    volume: f32,
    offset: Duration,
    rate: f32,
) -> Result<()> {
    let replacement = playback_from_url(
        stream,
        tools,
        playback.media_url.clone(),
        playback.bitrate_kbps,
        volume,
        offset,
        rate,
        playback.is_paused(),
    )?;
    playback.stop();
    *playback = replacement;
    Ok(())
}

fn parse_sponsor_segments(text: &str) -> Vec<SponsorSegment> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(text.trim()) else {
        return Vec::new();
    };
    let mut segments: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| {
            let segment = entry.get("segment")?.as_array()?;
            let start = segment.first()?.as_f64()?;
            let end = segment.get(1)?.as_f64()?;
            (start.is_finite() && end.is_finite() && start >= 0.0 && end > start).then(|| {
                SponsorSegment {
                    start: Duration::from_secs_f64(start),
                    end: Duration::from_secs_f64(end),
                }
            })
        })
        .collect();
    segments.sort_by_key(|segment| segment.start);
    segments.dedup();
    segments
}

fn resolve_sponsor_segments(
    track: &YouTubeTrack,
    categories_json: &str,
) -> Result<Vec<SponsorSegment>> {
    if categories_json == "[]" {
        return Ok(Vec::new());
    }
    let output = Command::new("curl.exe")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--get",
            SPONSORBLOCK_API,
            "--data-urlencode",
            &format!("videoID={}", track.video_id),
            "--data-urlencode",
            &format!("categories={categories_json}"),
        ])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to query SponsorBlock for {}", track.title))?;
    if !output.status.success() {
        bail!("SponsorBlock metadata unavailable");
    }
    Ok(parse_sponsor_segments(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn sponsor_skip_target(
    segments: &[SponsorSegment],
    elapsed: Duration,
    last_skipped: Option<&SponsorSegment>,
) -> Option<SponsorSegment> {
    segments
        .iter()
        .find(|segment| {
            segment.start <= elapsed && elapsed < segment.end && Some(*segment) != last_skipped
        })
        .cloned()
}

fn cache_sponsor_segments(track: &mut YouTubeTrack, categories_json: &str) {
    if track.sponsor_segments.is_none() {
        track.sponsor_segments =
            Some(resolve_sponsor_segments(track, categories_json).unwrap_or_default());
    }
}

fn play_youtube_tracks(
    mut tracks: Vec<YouTubeTrack>,
    tools: &YouTubeTools,
    sponsor_settings: &SponsorBlockSettings,
) -> Result<PlayerExit> {
    let mut raw = Some(RawMode::new()?);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    let mut output = AudioOutput::open()?;
    let media_controls = MediaControls::new();
    let mut original_tracks = tracks.clone();
    let mut index = 0usize;
    let mut resume_after_replay = None;
    let mut play_next = 0usize;
    let mut volume = load_volume();
    let mut last_volume_change = None;
    let mut shuffle = false;
    let mut loop_mode = LoopMode::Off;
    let mut rate = 1.0;
    let categories_json = sponsorblock_categories_json(sponsor_settings);
    let mut sponsor_enabled = categories_json != "[]";
    let mut last_skipped = None;
    let mut stream_failures = 0usize;
    let mut transcript_cache = HashMap::<String, Option<Lyrics>>::new();
    let mut chapter_cache = HashMap::<String, ChapterLoadState>::new();
    let (chapter_sender, chapter_receiver) = mpsc::channel();
    let mut chapter_menu: Option<ChaptersMenu> = None;
    let mut transcript_visible = load_setting_bool("playback.transcript", true);
    let mut transcript_settings = None;
    cache_sponsor_segments(&mut tracks[index], &categories_json);
    let mut playback = start_track(output.stream(), tools, &tracks[index], volume, rate)?;
    let mut resumed_from_suspend = false;
    media_controls.set_track(&tracks[index].title, "YouTube");
    media_controls.set_playing(true);

    loop {
        while let Ok((video_id, result)) = chapter_receiver.try_recv() {
            if matches!(
                chapter_cache.get(&video_id),
                Some(ChapterLoadState::Loading)
            ) {
                let state = match result {
                    Ok(chapters) => ChapterLoadState::Ready(chapters),
                    Err(error) => ChapterLoadState::Failed(error),
                };
                if let (Some(menu), ChapterLoadState::Ready(chapters)) = (&mut chapter_menu, &state)
                    && menu.video_id == video_id
                {
                    menu.selected =
                        active_chapter(chapters, playback.elapsed(tracks[index].duration))
                            .unwrap_or(0);
                    menu.scroll = 0;
                }
                chapter_cache.insert(video_id, state);
            }
        }
        if chapter_menu
            .as_ref()
            .is_some_and(|menu| menu.video_id != tracks[index].video_id)
        {
            chapter_menu = None;
        }
        if transcript_visible {
            ensure_transcript(&mut transcript_cache, tools, &tracks[index]);
        }
        let elapsed = playback.elapsed(tracks[index].duration);
        let stream_ended_early =
            playback.empty() && playback_ended_early(elapsed, tracks[index].duration);
        if stream_ended_early {
            stream_failures += 1;
        } else if elapsed >= Duration::from_secs(2) {
            stream_failures = 0;
        }
        let interruption =
            playback_interruption(output.is_lost(), resumed_from_suspend, stream_ended_early);
        resumed_from_suspend = false;
        if let Some(mut interruption) = interruption {
            let mut offset = elapsed;
            let mut paused = playback.is_paused();
            let mut changed_track = false;
            playback.stop();
            media_controls.set_playing(false);
            loop {
                draw_panel(
                    &mut stdout,
                    "Music Terminal Player · YouTube",
                    &[
                        format!(
                            "Track {}/{} | {}",
                            index + 1,
                            tracks.len(),
                            bold_text(&tracks[index].title)
                        ),
                        String::new(),
                        recovery_message(interruption, stream_failures).to_string(),
                        "[p/Space] pause/retry · [n/v] track · [b] back · [q] quit".to_string(),
                    ],
                )?;

                let can_retry = interruption != PlaybackInterruption::Stream || stream_failures < 3;
                if can_retry {
                    let replacement_playback = |stream: &OutputStream| {
                        if changed_track {
                            start_track(stream, tools, &tracks[index], volume, rate).inspect(
                                |replacement| {
                                    if paused {
                                        replacement.pause();
                                    }
                                },
                            )
                        } else {
                            resolve_audio_url(tools, &tracks[index]).and_then(|resolved| {
                                playback_from_url(
                                    stream,
                                    tools,
                                    resolved.url,
                                    resolved.bitrate_kbps,
                                    volume,
                                    offset,
                                    rate,
                                    paused,
                                )
                            })
                        }
                    };

                    if interruption == PlaybackInterruption::Stream {
                        if let Ok(replacement) = replacement_playback(output.stream()) {
                            playback = replacement;
                            media_controls.set_track(&tracks[index].title, "YouTube");
                            media_controls.set_playing(!paused);
                            break;
                        }
                    } else if let Ok(replacement_output) = AudioOutput::open()
                        && let Ok(replacement) = replacement_playback(replacement_output.stream())
                    {
                        output = replacement_output;
                        playback = replacement;
                        media_controls.set_track(&tracks[index].title, "YouTube");
                        media_controls.set_playing(!paused);
                        break;
                    }
                }

                if let Some(command) = media_controls.command() {
                    match command {
                        MediaCommand::Play => paused = false,
                        MediaCommand::Pause => paused = true,
                        MediaCommand::Next => {
                            let Some((next, new_pass, advance_queue)) = forward_track_index(
                                index,
                                tracks.len(),
                                loop_mode,
                                false,
                                &mut resume_after_replay,
                            ) else {
                                return Ok(PlayerExit::Back);
                            };
                            if new_pass {
                                restart_pass_order(&mut tracks, &original_tracks, shuffle);
                            }
                            if advance_queue && play_next > 0 {
                                play_next -= 1;
                            }
                            index = next;
                            changed_track = true;
                            interruption = PlaybackInterruption::Stream;
                            stream_failures = 0;
                            cache_sponsor_segments(&mut tracks[index], &categories_json);
                            offset = Duration::ZERO;
                            last_skipped = None;
                            media_controls.set_track(&tracks[index].title, "YouTube");
                        }
                        MediaCommand::Previous => {
                            index =
                                previous_track_index(index, tracks.len(), &mut resume_after_replay);
                            changed_track = true;
                            interruption = PlaybackInterruption::Stream;
                            stream_failures = 0;
                            cache_sponsor_segments(&mut tracks[index], &categories_json);
                            offset = Duration::ZERO;
                            last_skipped = None;
                            media_controls.set_track(&tracks[index].title, "YouTube");
                        }
                    }
                }

                if event::poll(Duration::from_millis(500))? {
                    let Event::Key(key) = event::read()? else {
                        continue;
                    };
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') => return Ok(PlayerExit::Quit),
                        KeyCode::Char('b') | KeyCode::Esc => return Ok(PlayerExit::Back),
                        KeyCode::Char('/') => toggle_playback_key_bindings()?,
                        KeyCode::Char('p') | KeyCode::Char(' ') if stream_failures >= 3 => {
                            stream_failures = 0;
                        }
                        KeyCode::Char('p') | KeyCode::Char(' ') => paused = !paused,
                        KeyCode::Char('n') => {
                            let Some((next, new_pass, advance_queue)) = forward_track_index(
                                index,
                                tracks.len(),
                                loop_mode,
                                false,
                                &mut resume_after_replay,
                            ) else {
                                return Ok(PlayerExit::Back);
                            };
                            if new_pass {
                                restart_pass_order(&mut tracks, &original_tracks, shuffle);
                            }
                            if advance_queue && play_next > 0 {
                                play_next -= 1;
                            }
                            index = next;
                            changed_track = true;
                            interruption = PlaybackInterruption::Stream;
                            stream_failures = 0;
                            cache_sponsor_segments(&mut tracks[index], &categories_json);
                            offset = Duration::ZERO;
                            last_skipped = None;
                            media_controls.set_track(&tracks[index].title, "YouTube");
                        }
                        KeyCode::Char('v') if key.modifiers.is_empty() => {
                            index =
                                previous_track_index(index, tracks.len(), &mut resume_after_replay);
                            changed_track = true;
                            interruption = PlaybackInterruption::Stream;
                            stream_failures = 0;
                            cache_sponsor_segments(&mut tracks[index], &categories_json);
                            offset = Duration::ZERO;
                            last_skipped = None;
                            media_controls.set_track(&tracks[index].title, "YouTube");
                        }
                        _ => {}
                    }
                }
            }
            continue;
        }

        if let Some(command) = media_controls.command() {
            match command {
                MediaCommand::Play => {
                    playback.play();
                    media_controls.set_playing(true);
                }
                MediaCommand::Pause => {
                    playback.pause();
                    media_controls.set_playing(false);
                }
                MediaCommand::Next => {
                    let Some((next, new_pass, advance_queue)) = forward_track_index(
                        index,
                        tracks.len(),
                        loop_mode,
                        false,
                        &mut resume_after_replay,
                    ) else {
                        return Ok(PlayerExit::Back);
                    };
                    if new_pass {
                        restart_pass_order(&mut tracks, &original_tracks, shuffle);
                    }
                    if advance_queue && play_next > 0 {
                        play_next -= 1;
                    }
                    index = next;
                    cache_sponsor_segments(&mut tracks[index], &categories_json);
                    last_skipped = None;
                    replace_track(
                        &mut playback,
                        output.stream(),
                        tools,
                        &tracks[index],
                        volume,
                        rate,
                    )?;
                    media_controls.set_track(&tracks[index].title, "YouTube");
                    media_controls.set_playing(true);
                }
                MediaCommand::Previous => {
                    index = previous_track_index(index, tracks.len(), &mut resume_after_replay);
                    cache_sponsor_segments(&mut tracks[index], &categories_json);
                    last_skipped = None;
                    replace_track(
                        &mut playback,
                        output.stream(),
                        tools,
                        &tracks[index],
                        volume,
                        rate,
                    )?;
                    media_controls.set_track(&tracks[index].title, "YouTube");
                    media_controls.set_playing(true);
                }
            }
            continue;
        }

        if sponsor_enabled {
            let elapsed = playback.elapsed(tracks[index].duration);
            if let Some(segment) = sponsor_skip_target(
                tracks[index]
                    .sponsor_segments
                    .as_deref()
                    .unwrap_or_default(),
                elapsed,
                last_skipped.as_ref(),
            ) {
                restart_playback(
                    &mut playback,
                    output.stream(),
                    tools,
                    volume,
                    segment.end,
                    rate,
                )?;
                last_skipped = Some(segment);
                continue;
            }
        }

        if let Some(menu) = chapter_menu.as_mut() {
            if let Some(state) = chapter_cache.get(&menu.video_id) {
                draw_chapters_menu(
                    &mut stdout,
                    menu,
                    state,
                    playback.elapsed(tracks[index].duration),
                )?;
            }
        } else {
            draw_youtube_player(
                &mut stdout,
                &tracks,
                index,
                &playback,
                volume,
                last_volume_change,
                shuffle,
                loop_mode,
                sponsor_enabled,
                cached_transcript(&transcript_cache, &tracks[index]),
                transcript_visible,
                transcript_settings.as_ref(),
            )?;
        }

        if playback.empty() {
            let Some((next, new_pass, advance_queue)) = forward_track_index(
                index,
                tracks.len(),
                loop_mode,
                true,
                &mut resume_after_replay,
            ) else {
                return Ok(PlayerExit::Back);
            };
            if new_pass {
                restart_pass_order(&mut tracks, &original_tracks, shuffle);
            }
            if advance_queue && play_next > 0 && loop_mode != LoopMode::One {
                play_next -= 1;
            }
            index = next;
            cache_sponsor_segments(&mut tracks[index], &categories_json);
            last_skipped = None;
            replace_track(
                &mut playback,
                output.stream(),
                tools,
                &tracks[index],
                volume,
                rate,
            )?;
            media_controls.set_track(&tracks[index].title, "YouTube");
            media_controls.set_playing(true);
            continue;
        }

        let poll_started = Instant::now();
        let event_ready = event::poll(Duration::from_millis(200))?;
        if resume_gap_detected(poll_started.elapsed()) {
            resumed_from_suspend = true;
            continue;
        }
        if !event_ready {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(PlayerExit::Quit);
        }

        if let Some(menu) = chapter_menu.as_mut() {
            let state = chapter_cache.get(&menu.video_id);
            match key.code {
                KeyCode::Esc => chapter_menu = None,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    if let Some(ChapterLoadState::Ready(chapters)) = state
                        && !chapters.is_empty()
                    {
                        let page = chapter_page_size(terminal::size().unwrap_or((80, 24)).1);
                        let last = chapters.len() - 1;
                        menu.selected = match key.code {
                            KeyCode::Up => menu.selected.saturating_sub(1),
                            KeyCode::Down => menu.selected.saturating_add(1).min(last),
                            KeyCode::PageUp => menu.selected.saturating_sub(page),
                            KeyCode::PageDown => menu.selected.saturating_add(page).min(last),
                            _ => menu.selected,
                        };
                        menu.seek_error = None;
                    }
                }
                KeyCode::Char('r') | KeyCode::Enter
                    if matches!(state, Some(ChapterLoadState::Failed(_))) =>
                {
                    request_chapters(&mut chapter_cache, &chapter_sender, tools, &tracks[index]);
                    menu.seek_error = None;
                }
                KeyCode::Enter => {
                    if let Some(ChapterLoadState::Ready(chapters)) = state
                        && let Some(chapter) = chapters.get(menu.selected)
                    {
                        match restart_playback(
                            &mut playback,
                            output.stream(),
                            tools,
                            volume,
                            chapter.start,
                            rate,
                        ) {
                            Ok(()) => {
                                last_skipped = None;
                                chapter_menu = None;
                            }
                            Err(error) => menu.seek_error = Some(error.to_string()),
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        if let Some(editor) = transcript_settings.as_mut() {
            match editor.handle_key(key.code, key.modifiers, transcript_visible)? {
                TranscriptEditorResult::Visibility(visible) => {
                    transcript_visible = visible;
                    continue;
                }
                TranscriptEditorResult::LanguageChanged => continue,
                TranscriptEditorResult::Close => {
                    transcript_settings = None;
                    continue;
                }
                TranscriptEditorResult::Handled => continue,
                TranscriptEditorResult::Pass => {}
            }
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.is_empty() => {
                let track = &tracks[index];
                if !chapter_cache.contains_key(&track.video_id) {
                    request_chapters(&mut chapter_cache, &chapter_sender, tools, track);
                }
                let selected = match chapter_cache.get(&track.video_id) {
                    Some(ChapterLoadState::Ready(chapters)) => {
                        active_chapter(chapters, playback.elapsed(track.duration)).unwrap_or(0)
                    }
                    _ => 0,
                };
                chapter_menu = Some(ChaptersMenu {
                    video_id: track.video_id.clone(),
                    selected,
                    scroll: 0,
                    seek_error: None,
                });
            }
            KeyCode::Char('q') => return Ok(PlayerExit::Quit),
            KeyCode::Char('b') => {
                transcript_settings = Some(TranscriptSettingsEditor::new());
            }
            KeyCode::Esc => return Ok(PlayerExit::Back),
            KeyCode::Char('y') if key.modifiers.is_empty() => {
                transcript_visible = !transcript_visible;
                save_setting_bool("playback.transcript", transcript_visible)?;
            }
            KeyCode::Char('/') => toggle_playback_key_bindings()?,
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                if playback.is_paused() {
                    playback.play();
                    media_controls.set_playing(true);
                } else {
                    playback.pause();
                    media_controls.set_playing(false);
                }
            }
            KeyCode::Char('n') => {
                let Some((next, new_pass, advance_queue)) = forward_track_index(
                    index,
                    tracks.len(),
                    loop_mode,
                    false,
                    &mut resume_after_replay,
                ) else {
                    return Ok(PlayerExit::Back);
                };
                if new_pass {
                    restart_pass_order(&mut tracks, &original_tracks, shuffle);
                }
                if advance_queue && play_next > 0 {
                    play_next -= 1;
                }
                index = next;
                cache_sponsor_segments(&mut tracks[index], &categories_json);
                last_skipped = None;
                replace_track(
                    &mut playback,
                    output.stream(),
                    tools,
                    &tracks[index],
                    volume,
                    rate,
                )?;
                media_controls.set_track(&tracks[index].title, "YouTube");
                media_controls.set_playing(true);
            }
            KeyCode::Char('v') if key.modifiers.is_empty() => {
                index = previous_track_index(index, tracks.len(), &mut resume_after_replay);
                cache_sponsor_segments(&mut tracks[index], &categories_json);
                last_skipped = None;
                replace_track(
                    &mut playback,
                    output.stream(),
                    tools,
                    &tracks[index],
                    volume,
                    rate,
                )?;
                media_controls.set_track(&tracks[index].title, "YouTube");
                media_controls.set_playing(true);
            }
            KeyCode::Left | KeyCode::Right => {
                let target = seek_target(
                    playback.elapsed(tracks[index].duration),
                    tracks[index].duration,
                    key.code == KeyCode::Right,
                );
                last_skipped = None;
                restart_playback(&mut playback, output.stream(), tools, volume, target, rate)?;
            }
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.10).min(MAX_VOLUME);
                last_volume_change = Some(Instant::now());
                playback.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Down | KeyCode::Char('-') => {
                volume = (volume - 0.10).max(0.0);
                last_volume_change = Some(Instant::now());
                playback.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Char(',') | KeyCode::Char('.') => {
                let next_rate = stepped_rate(rate, key.code == KeyCode::Char('.'));
                if next_rate != rate {
                    let elapsed = playback.elapsed(tracks[index].duration);
                    rate = next_rate;
                    restart_playback(&mut playback, output.stream(), tools, volume, elapsed, rate)?;
                }
            }
            KeyCode::Char('s') => sponsor_enabled = !sponsor_enabled,
            KeyCode::Char('l') => loop_mode = loop_mode.cycle(),
            KeyCode::Char('r') => {
                resume_after_replay = None;
                shuffle = !shuffle;
                set_shuffle_order(&mut tracks, index, play_next, &original_tracks, shuffle);
            }
            KeyCode::Char('a') => {
                drop(raw.take());
                let additions = prompt_and_resolve_tracks(tools);
                raw = Some(RawMode::new()?);
                let additions = additions?;
                if !additions.is_empty() {
                    insert_queue_next(&mut tracks, index, &mut play_next, additions.clone());
                    original_tracks.extend(additions);
                    shuffle = false;
                }
            }
            KeyCode::Char('u') => loop {
                let edit = manage_queue_with_adder(
                    &mut tracks,
                    index,
                    &mut play_next,
                    |track| track.title.clone(),
                    || {
                        drop(raw.take());
                        let additions = prompt_and_resolve_tracks(tools);
                        raw = Some(RawMode::new()?);
                        let additions = additions?;
                        original_tracks.extend(additions.iter().cloned());
                        Ok(additions)
                    },
                    || playback.empty(),
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
                        return Ok(PlayerExit::Back);
                    };
                    if new_pass {
                        restart_pass_order(&mut tracks, &original_tracks, shuffle);
                    }
                    if advance_queue && play_next > 0 && loop_mode != LoopMode::One {
                        play_next -= 1;
                    }
                    index = next;
                    cache_sponsor_segments(&mut tracks[index], &categories_json);
                    last_skipped = None;
                    replace_track(
                        &mut playback,
                        output.stream(),
                        tools,
                        &tracks[index],
                        volume,
                        rate,
                    )?;
                    media_controls.set_track(&tracks[index].title, "YouTube");
                    media_controls.set_playing(true);
                    continue;
                }
                if edit.restart {
                    resume_after_replay = None;
                    play_next = 0;
                    cache_sponsor_segments(&mut tracks[index], &categories_json);
                    last_skipped = None;
                    replace_track(
                        &mut playback,
                        output.stream(),
                        tools,
                        &tracks[index],
                        volume,
                        rate,
                    )?;
                    media_controls.set_track(&tracks[index].title, "YouTube");
                    media_controls.set_playing(true);
                }
                break;
            },
            _ => {}
        }
    }
}

fn draw_youtube_player(
    stdout: &mut io::Stdout,
    tracks: &[YouTubeTrack],
    index: usize,
    playback: &YouTubePlayback,
    volume: f32,
    last_volume_change: Option<Instant>,
    shuffle: bool,
    loop_mode: LoopMode,
    sponsor_enabled: bool,
    transcript: Option<&Lyrics>,
    transcript_visible: bool,
    transcript_settings: Option<&TranscriptSettingsEditor>,
) -> Result<()> {
    let state = if playback.is_paused() {
        tr("msg.paused")
    } else {
        tr("msg.playing")
    };
    let duration = tracks[index].duration;
    let elapsed = playback.elapsed(duration);
    let total = duration
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let time_text = format!("{}/{}", format_duration(elapsed), total);
    let mut rows = vec![
        player_title_row(tr("msg.track"), index, tracks.len(), &tracks[index].title),
        String::new(),
    ];
    let progress = playback_progress_or_volume(
        format!(
            "[{}] {}/{}",
            progress_bar(elapsed, duration, playback_bar_width(&time_text)),
            format_duration(elapsed),
            total,
        ),
        volume,
        last_volume_change,
    );
    let showing_volume = last_volume_change.is_some_and(|at| at.elapsed() < Duration::from_secs(1));
    rows.push(progress);
    if !showing_volume {
        let details = format!(
            "{:.2}x{}",
            playback.rate,
            playback
                .bitrate_kbps
                .map(|kbps| format!(" · {kbps} kbps"))
                .unwrap_or_default()
        );
        let bar_row = rows.last_mut().expect("progress row exists");
        if crate::util::playback_metadata_fits(bar_row, &details) {
            bar_row.push_str(" · ");
            bar_row.push_str(&details);
        } else {
            rows.extend(wrap_playback_status(&details));
        }
    }
    rows.extend(wrap_playback_status(&format!(
        "{} · {} {} · {} {}",
        state,
        tr("msg.shuffle"),
        if shuffle { tr("msg.on") } else { tr("msg.off") },
        tr("msg.loop"),
        loop_mode.label(),
    )));
    rows.extend(wrap_playback_status(&format!(
        "SponsorBlock {} · {}",
        if sponsor_enabled {
            tr("msg.on")
        } else {
            tr("msg.off")
        },
        tr("msg.a_add_next_url_or_playlist")
    )));
    if load_setting_bool("playback.key_bindings", true) {
        rows.push(playback_key_bindings_hint());
        rows.extend(youtube_playback_controls());
    }
    draw_transcript_player_panels(
        stdout,
        "Music Terminal Player · YouTube",
        &rows,
        transcript,
        elapsed,
        transcript_visible,
        transcript_settings,
    )
}

fn youtube_playback_controls() -> Vec<String> {
    playback_controls_with_youtube()
}

#[cfg(test)]
mod youtube_tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use unicode_width::UnicodeWidthStr;

    use crate::lyrics::Lyrics;

    use super::{
        ChapterLoadState, ChaptersMenu, PlaybackInterruption, SPONSORBLOCK_CATEGORIES,
        SponsorBlockSettings, SponsorSegment, TranscriptChoice, YouTubeTools, active_chapter,
        chapter_menu_rows, choose_transcript, compare_versions, install_staged_tool,
        installed_version, is_portable_ffmpeg, is_portable_yt_dlp, logical_elapsed,
        parse_resolved_audio_stream, parse_sponsor_segments, parse_sponsorblock_settings,
        parse_webvtt_transcript, parse_youtube_chapters, parse_youtube_track_line,
        parse_yt_dlp_release, playback_ended_early, playback_interruption,
        portable_ffmpeg_update_paths, portable_tool_pair, portable_yt_dlp_update_paths,
        recovery_message, relocated_portable_tools, resume_gap_detected, seek_target,
        serialize_sponsorblock_settings, sponsor_skip_target, sponsorblock_categories_json,
        stepped_rate, update_status, youtube_extractor_args, youtube_playback_controls,
        youtube_stream_format,
    };

    #[test]
    fn youtube_chapters_parse_and_sort_full_metadata() {
        let metadata = serde_json::json!({
            "chapters": [
                {"title": "Kết thúc", "start_time": 130.5, "end_time": 190},
                {"title": "Introduction", "start_time": 0, "end_time": 60},
                {"title": "Bài hát thứ hai", "start_time": 60, "end_time": 130.5},
                {"title": "Duplicate timestamp", "start_time": 60, "end_time": 100}
            ]
        });
        let chapters = parse_youtube_chapters(&metadata);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0].title, "Introduction");
        assert_eq!(chapters[1].title, "Bài hát thứ hai");
        assert_eq!(chapters[1].start, Duration::from_secs(60));
        assert_eq!(chapters[2].start, Duration::from_millis(130500));
        assert_eq!(chapters[2].end, Some(Duration::from_secs(190)));
        assert!(parse_youtube_chapters(&serde_json::json!({})).is_empty());
        assert!(parse_youtube_chapters(&serde_json::json!({"chapters": []})).is_empty());
    }

    #[test]
    fn youtube_chapters_ignore_invalid_entries_and_find_active_segment() {
        let metadata = serde_json::json!({
            "chapters": [
                {"title": "", "start_time": 0, "end_time": 10},
                {"title": "Negative", "start_time": -1, "end_time": 3},
                {"title": "Missing start"},
                {"title": "Bad end", "start_time": 5, "end_time": 4},
                {"title": "Not numeric", "start_time": "10"},
                {"title": "First", "start_time": 0, "end_time": 10},
                {"title": "Later", "start_time": 20, "end_time": 30}
            ]
        });
        let chapters = parse_youtube_chapters(&metadata);
        assert_eq!(chapters.len(), 2);
        assert_eq!(active_chapter(&chapters, Duration::ZERO), Some(0));
        assert_eq!(active_chapter(&chapters, Duration::from_secs(10)), None);
        assert_eq!(active_chapter(&chapters, Duration::from_secs(20)), Some(1));
        assert_eq!(active_chapter(&chapters, Duration::from_secs(30)), None);
    }

    #[test]
    fn chapter_menu_is_responsive_and_scrolls_to_selection() {
        let chapters = (0..40)
            .map(|index| super::YouTubeChapter {
                title: format!("Chương {index}: Bài hát dài có tiếng Việt và emoji 🎶"),
                start: Duration::from_secs(index * 60),
                end: Some(Duration::from_secs((index + 1) * 60)),
            })
            .collect::<Vec<_>>();
        let state = ChapterLoadState::Ready(chapters);
        for columns in [20, 24, 40, 52, 80, 120] {
            for height in [8, 12, 24] {
                let mut menu = ChaptersMenu {
                    video_id: "video".to_string(),
                    selected: 35,
                    scroll: 0,
                    seek_error: None,
                };
                let rows = chapter_menu_rows(
                    &mut menu,
                    &state,
                    Duration::from_secs(35 * 60),
                    columns,
                    height,
                );
                let max_width = usize::from(columns).saturating_sub(5).min(60);
                assert!(
                    rows.iter()
                        .all(|row| UnicodeWidthStr::width(row.as_str()) <= max_width)
                );
                assert!(rows.iter().any(|row| row.starts_with("> 35:00")));
                assert!(rows.iter().any(|row| row.contains('♪')));
                assert!(rows.len() + 4 < usize::from(height));
                assert!(menu.scroll <= menu.selected);
            }
        }

        let mut menu = ChaptersMenu {
            video_id: "empty".to_string(),
            selected: 0,
            scroll: 0,
            seek_error: None,
        };
        let no_chapters = chapter_menu_rows(
            &mut menu,
            &ChapterLoadState::Ready(Vec::new()),
            Duration::ZERO,
            52,
            24,
        );
        assert!(
            no_chapters
                .iter()
                .any(|row| row.contains("No chapters available"))
        );
        let failed = chapter_menu_rows(
            &mut menu,
            &ChapterLoadState::Failed("Network unavailable".to_string()),
            Duration::ZERO,
            52,
            24,
        );
        assert!(failed.iter().any(|row| row.contains("Retry")));
        let failed_small = chapter_menu_rows(
            &mut menu,
            &ChapterLoadState::Failed("Network unavailable".to_string()),
            Duration::ZERO,
            20,
            8,
        );
        assert!(failed_small.iter().any(|row| row.contains("Retry")));
    }

    #[test]
    fn metadata_line_parses_duration_and_missing_duration() {
        let track =
            parse_youtube_track_line("abc\thttps://youtu.be/abc\tExample title\t123.5").unwrap();
        assert_eq!(track.video_id, "abc");
        assert_eq!(track.title, "Example title");
        assert_eq!(track.duration.unwrap().as_secs_f64(), 123.5);

        let live =
            parse_youtube_track_line("live\thttps://youtu.be/live\tLive stream\tNA").unwrap();
        assert_eq!(live.duration, None);

        let untitled = parse_youtube_track_line(
            "l-vSSYEuO88\thttps://www.youtube.com/watch?v=l-vSSYEuO88\t\t8707",
        )
        .unwrap();
        assert_eq!(untitled.title, "YouTube video l-vSSYEuO88");
    }

    #[test]
    fn playback_time_helpers_clamp_seek_rate_and_logical_elapsed() {
        assert_eq!(
            logical_elapsed(
                Duration::from_secs(20),
                Duration::from_secs(5),
                1.5,
                Some(Duration::from_secs(25)),
            ),
            Duration::from_secs(25)
        );
        assert_eq!(
            seek_target(Duration::from_secs(4), None, false),
            Duration::ZERO
        );
        assert_eq!(
            seek_target(Duration::from_secs(55), Some(Duration::from_secs(60)), true,),
            Duration::from_secs(60)
        );
        assert_eq!(stepped_rate(0.5, false), 0.5);
        assert_eq!(stepped_rate(2.0, true), 2.0);
        assert_eq!(stepped_rate(1.0, true), 1.25);
    }

    #[test]
    fn suspend_gap_and_premature_end_trigger_recovery() {
        assert!(!resume_gap_detected(Duration::from_secs(4)));
        assert!(resume_gap_detected(Duration::from_secs(5)));
        assert!(playback_ended_early(
            Duration::from_secs(30),
            Some(Duration::from_secs(60))
        ));
        assert!(!playback_ended_early(
            Duration::from_secs(55),
            Some(Duration::from_secs(60))
        ));
        assert!(!playback_ended_early(Duration::from_secs(30), None));
    }

    #[test]
    fn youtube_stream_uses_android_client_and_classifies_recovery() {
        assert_eq!(youtube_stream_format(), "251/140/18/bestaudio/best");
        assert_eq!(youtube_extractor_args(), "youtube:player_client=android");
        let resolved =
            parse_resolved_audio_stream("251\thttps://example.test/audio\t146.721").unwrap();
        assert_eq!(resolved.url, "https://example.test/audio");
        assert_eq!(resolved.bitrate_kbps, Some(147));
        assert_eq!(
            parse_resolved_audio_stream("251\thttps://example.test/audio\tNA")
                .unwrap()
                .bitrate_kbps,
            Some(160)
        );
        assert_eq!(
            parse_resolved_audio_stream("140\thttps://example.test/audio\tNA")
                .unwrap()
                .bitrate_kbps,
            Some(128)
        );
        assert_eq!(
            playback_interruption(false, false, true),
            Some(PlaybackInterruption::Stream)
        );
        assert_eq!(
            recovery_message(PlaybackInterruption::Stream, 3),
            "YouTube stream unavailable. Press p/Space to retry or choose another track."
        );
        assert_eq!(
            playback_interruption(true, false, true),
            Some(PlaybackInterruption::AudioOutput)
        );
    }

    #[test]
    fn sponsorblock_defaults_preserve_existing_categories() {
        let settings = SponsorBlockSettings::default();
        assert_eq!(
            SPONSORBLOCK_CATEGORIES.map(|category| category.id),
            [
                "sponsor",
                "music_offtopic",
                "interaction",
                "intro",
                "outro",
                "preview",
            ]
        );
        assert_eq!(settings.enabled, [true, true, false, false, false, false]);
        assert_eq!(
            sponsorblock_categories_json(&settings),
            r#"["sponsor","music_offtopic"]"#
        );
    }

    #[test]
    fn sponsorblock_settings_parse_serialize_and_select_categories() {
        let settings = parse_sponsorblock_settings(
            "sponsor=no_skip\nmusic_offtopic=auto_skip\ninteraction=auto_skip\nintro=auto_skip\noutro=auto_skip\npreview=auto_skip\nunknown=auto_skip\n",
        );
        assert_eq!(settings.enabled, [false, true, true, true, true, true]);
        assert_eq!(
            sponsorblock_categories_json(&settings),
            r#"["music_offtopic","interaction","intro","outro","preview"]"#
        );
        assert_eq!(
            parse_sponsorblock_settings(&serialize_sponsorblock_settings(&settings)),
            settings
        );

        let disabled = SponsorBlockSettings {
            enabled: [false; 6],
        };
        assert_eq!(sponsorblock_categories_json(&disabled), "[]");
    }

    #[test]
    fn sponsor_segments_parse_and_do_not_repeat() {
        let json = r#"[{"category":"music_offtopic","segment":[10.5,20]}]"#;
        let segments = parse_sponsor_segments(json);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, Duration::from_secs_f64(10.5));
        assert_eq!(
            sponsor_skip_target(&segments, Duration::from_secs(15), None),
            Some(segments[0].clone())
        );
        assert_eq!(
            sponsor_skip_target(&segments, Duration::from_secs(15), Some(&segments[0])),
            None
        );
        assert!(parse_sponsor_segments("invalid").is_empty());
    }

    #[test]
    fn tool_update_versions_parse_compare_and_report_status() {
        assert_eq!(
            parse_yt_dlp_release(r#"{"tag_name":"2026.07.04"}"#).as_deref(),
            Some("2026.07.04")
        );
        assert_eq!(parse_yt_dlp_release("invalid"), None);
        assert_eq!(
            installed_version(
                "ffmpeg version 9.0.1-essentials_build-www.gyan.dev Copyright FFmpeg",
                "FFmpeg",
            ),
            "9.0.1"
        );
        assert_eq!(installed_version("2026.07.04\n", "yt-dlp"), "2026.07.04");

        assert_eq!(
            compare_versions("2026.07.03", "2026.07.04"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("9.0.1", "8.1.2"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("1.0.1", "1"), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions("1.0", "1"), std::cmp::Ordering::Equal);

        assert_eq!(update_status("8.1.2", None, false), "not checked");
        assert_eq!(update_status("8.1.2", None, true), "unable to check");
        assert_eq!(update_status("9.0.1", Some("9.0.1"), true), "latest");
        assert_eq!(
            update_status("8.1.2", Some("9.0.1"), true),
            "update available: 9.0.1"
        );
        assert_eq!(
            update_status("9.1", Some("9.0.1"), true),
            "newer than stable 9.0.1"
        );
        assert_eq!(
            update_status("unavailable", Some("9.0.1"), true),
            "latest 9.0.1; installed version unavailable"
        );
    }

    #[test]
    fn updater_only_accepts_executable_relative_portable_tools() {
        let executable = Path::new(r"C:\Player\music-terminal-player.exe");
        assert!(is_portable_yt_dlp(
            r"C:\Player\tools\yt-dlp.exe",
            executable
        ));
        assert!(is_portable_ffmpeg(
            r"C:\Player\tools\ffmpeg.exe",
            executable
        ));
        assert!(!is_portable_yt_dlp("yt-dlp", executable));
        assert!(!is_portable_ffmpeg("ffmpeg", executable));
        assert!(!is_portable_yt_dlp(r"C:\Other\yt-dlp.exe", executable));
        assert!(!is_portable_ffmpeg(r"C:\Other\ffmpeg.exe", executable));

        let (download, backup) =
            portable_yt_dlp_update_paths(Path::new(r"C:\Player\tools\yt-dlp.exe"));
        assert_eq!(download, Path::new(r"C:\Player\tools\yt-dlp.exe.download"));
        assert_eq!(backup, Path::new(r"C:\Player\tools\yt-dlp.exe.backup"));

        let (archive, extracted, download, backup) =
            portable_ffmpeg_update_paths(Path::new(r"C:\Player\tools\ffmpeg.exe"));
        assert_eq!(archive, Path::new(r"C:\Player\tools\ffmpeg.zip"));
        assert_eq!(extracted, Path::new(r"C:\Player\tools\ffmpeg-extracted"));
        assert_eq!(download, Path::new(r"C:\Player\tools\ffmpeg.exe.download"));
        assert_eq!(backup, Path::new(r"C:\Player\tools\ffmpeg.exe.backup"));
    }

    #[test]
    fn failed_portable_tool_install_restores_existing_executable() {
        let root = std::env::temp_dir().join(format!(
            "music-terminal-player-updater-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("ffmpeg.exe");
        let download = root.join("missing.download");
        let backup = root.join("ffmpeg.exe.backup");
        fs::write(&target, b"existing").unwrap();

        assert!(install_staged_tool(&target, &download, &backup, "FFmpeg").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        assert!(!backup.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moved_portable_tools_rebase_when_only_sibling_ffmpeg_exists() {
        let root = std::env::temp_dir().join(format!(
            "music-terminal-player-relocation-{}",
            std::process::id()
        ));
        let moved = root.join("moved");
        let tools_dir = moved.join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(tools_dir.join("ffmpeg.exe"), b"").unwrap();

        let saved = YouTubeTools {
            yt_dlp: root
                .join("old")
                .join("tools")
                .join("yt-dlp.exe")
                .to_string_lossy()
                .into_owned(),
            ffmpeg: root
                .join("old")
                .join("tools")
                .join("ffmpeg.exe")
                .to_string_lossy()
                .into_owned(),
        };
        assert!(portable_tool_pair(&saved));
        let relocated =
            relocated_portable_tools(&saved, &moved.join("music-terminal-player.exe")).unwrap();
        assert_eq!(Path::new(&relocated.yt_dlp), tools_dir.join("yt-dlp.exe"));
        assert_eq!(Path::new(&relocated.ffmpeg), tools_dir.join("ffmpeg.exe"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relocation_rejects_path_commands_and_mixed_external_tools() {
        let executable = Path::new(r"C:\Player\music-terminal-player.exe");
        for tools in [
            YouTubeTools {
                yt_dlp: "yt-dlp".to_string(),
                ffmpeg: "ffmpeg".to_string(),
            },
            YouTubeTools {
                yt_dlp: r"C:\Player\tools\yt-dlp.exe".to_string(),
                ffmpeg: r"C:\FFmpeg\ffmpeg.exe".to_string(),
            },
        ] {
            assert!(!portable_tool_pair(&tools));
            assert!(relocated_portable_tools(&tools, executable).is_none());
        }
    }

    #[test]
    fn youtube_controls_add_seek_speed_and_sponsor_row() {
        let controls = youtube_playback_controls();
        assert!(controls.iter().any(|row| row.contains("[←/→] seek 10s")));
        assert!(controls.iter().any(|row| row.contains("[,/.] ±0.25x")));
        assert!(controls.iter().any(|row| row.contains("[s] sponsors")));
        assert!(controls.iter().any(|row| row.contains("[b] transcript")));
        assert!(controls.iter().any(|row| row.contains("[c] chapters")));
        assert!(controls.last().unwrap().starts_with('└'));
        assert!(controls.last().unwrap().ends_with('┘'));
    }

    #[test]
    fn webvtt_transcript_parses_timestamps_markup_and_entities() {
        let source = "WEBVTT\n\n00:00:01.250 --> 00:00:03.000\n<c>Hello &amp; welcome</c>\n\n00:03.500 --> 00:05.000\n<00:00:03.500><c>Next line</c>\n";
        let Lyrics::Timed(lines) = parse_webvtt_transcript(source).unwrap() else {
            panic!("expected timed transcript");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].at, Duration::from_millis(1250));
        assert_eq!(lines[0].text, "Hello & welcome");
        assert_eq!(lines[1].at, Duration::from_millis(3500));
        assert_eq!(lines[1].text, "Next line");
    }

    #[test]
    fn transcript_selection_prefers_manual_then_original_auto() {
        let metadata = serde_json::json!({
            "language": "ja",
            "subtitles": {"en": [{}], "ja": [{}]},
            "automatic_captions": {"en": [{}], "ja-orig": [{}], "vi": [{}]}
        });
        assert_eq!(
            choose_transcript(&metadata, "auto"),
            Some(TranscriptChoice {
                language: "ja".to_string(),
                automatic: false,
            })
        );
        assert_eq!(
            choose_transcript(&metadata, "vi"),
            Some(TranscriptChoice {
                language: "vi".to_string(),
                automatic: true,
            })
        );

        let automatic_only = serde_json::json!({
            "language": "ja",
            "subtitles": {},
            "automatic_captions": {"en": [{}], "ja-orig": [{}]}
        });
        assert_eq!(
            choose_transcript(&automatic_only, "original"),
            Some(TranscriptChoice {
                language: "ja-orig".to_string(),
                automatic: true,
            })
        );
    }

    #[test]
    fn sponsor_segment_model_rejects_outside_elapsed_time() {
        let segment = SponsorSegment {
            start: Duration::from_secs(10),
            end: Duration::from_secs(20),
        };
        assert_eq!(
            sponsor_skip_target(&[segment], Duration::from_secs(20), None),
            None
        );
    }

    #[test]
    fn youtube_authentication_settings_default_and_restore_method_and_profile() {
        use super::YouTubeAuthentication as Auth;

        assert_eq!(Auth::from_values(None, None, None), Auth::None);
        assert_eq!(Auth::from_values(Some("unknown"), None, None), Auth::None);
        assert_eq!(
            Auth::from_values(Some("thorium"), None, None),
            Auth::Thorium {
                profile: "Default".into()
            }
        );
        assert_eq!(
            Auth::from_values(Some("thorium"), Some("Profile 2"), None),
            Auth::Thorium {
                profile: "Profile 2".into()
            }
        );
        assert_eq!(
            Auth::from_values(Some("chrome"), Some("Profile 3"), None),
            Auth::Chrome {
                profile: "Profile 3".into()
            }
        );
        assert_eq!(
            Auth::from_values(Some("vivaldi"), None, None),
            Auth::Vivaldi {
                profile: "Default".into()
            }
        );
        assert_eq!(
            Auth::from_values(Some("zen"), Some("a1b2c3.default"), None),
            Auth::Zen {
                profile: "a1b2c3.default".into()
            }
        );
        assert_eq!(
            Auth::from_values(Some("file"), None, Some(r"C:\cookies.txt")),
            Auth::CookiesFile {
                path: r"C:\cookies.txt".into()
            }
        );
    }

    #[test]
    fn youtube_authentication_discovers_profiles_and_handles_missing_data() {
        use super::{YouTubeAuthentication as Auth, thorium_profiles, youtube_auth_arguments};

        let root = std::env::temp_dir().join(format!(
            "music-terminal-thorium-profiles-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(thorium_profiles(&root).is_err());
        for name in ["Profile 10", "Default", "Profile 2", "Cache"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        assert_eq!(
            thorium_profiles(&root).unwrap(),
            ["Default", "Profile 2", "Profile 10"]
        );

        let profile = Auth::Thorium {
            profile: "Profile 2".into(),
        };
        assert!(youtube_auth_arguments(&profile, Some(&root)).is_err());
        fs::create_dir_all(root.join("Profile 2").join("Network")).unwrap();
        fs::write(root.join("Profile 2").join("Network").join("Cookies"), b"").unwrap();
        let args = youtube_auth_arguments(&profile, Some(&root)).unwrap();
        assert_eq!(args[0], "--cookies-from-browser");
        assert_eq!(
            args[1],
            std::ffi::OsString::from(format!("chromium:{}", root.join("Profile 2").display()))
        );
        assert!(
            youtube_auth_arguments(
                &Auth::Thorium {
                    profile: "../Default".into()
                },
                Some(&root)
            )
            .is_err()
        );
        assert!(
            youtube_auth_arguments(
                &Auth::Thorium {
                    profile: "Profile 3".into()
                },
                Some(&root)
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn youtube_authentication_cookies_file_and_android_client_flags() {
        use super::{
            YouTubeAuthentication as Auth, append_youtube_auth_options, youtube_auth_arguments,
        };
        use std::process::Command;

        let mut command = Command::new("yt-dlp");
        append_youtube_auth_options(&mut command, &Auth::None, None, true).unwrap();
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            ["--extractor-args", "youtube:player_client=android"]
        );

        let path =
            std::env::temp_dir().join(format!("music-terminal-auth-{}.txt", std::process::id()));
        let auth = Auth::CookiesFile { path: path.clone() };
        assert!(youtube_auth_arguments(&auth, None).is_err());
        fs::write(&path, b"invalid cookies file\n").unwrap();
        assert!(youtube_auth_arguments(&auth, None).is_err());
        fs::write(&path, b"# Netscape HTTP Cookie File\n").unwrap();

        let mut command = Command::new("yt-dlp");
        append_youtube_auth_options(&mut command, &auth, None, true).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_os_string())
            .collect::<Vec<_>>();
        assert_eq!(args[0], "--cookies");
        assert_eq!(args[1], path.as_os_str());
        assert_eq!(args.len(), 2);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn youtube_authentication_new_browser_profiles_and_cookie_flags() {
        use super::{browser_auth_arguments, browser_profiles};

        let root = std::env::temp_dir().join(format!(
            "music-terminal-browser-profiles-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let chromium = root.join("chromium");
        let zen = root.join("zen");
        fs::create_dir_all(chromium.join("Default").join("Network")).unwrap();
        fs::create_dir_all(chromium.join("Profile 2").join("Network")).unwrap();
        fs::create_dir_all(zen.join("abc123.default-release")).unwrap();
        fs::write(zen.join("abc123.default-release").join("prefs.js"), b"").unwrap();

        assert_eq!(
            browser_profiles("chrome", &chromium).unwrap(),
            ["Default", "Profile 2"]
        );
        assert_eq!(
            browser_profiles("zen", &zen).unwrap(),
            ["abc123.default-release"]
        );
        assert!(browser_auth_arguments("chrome", "Default", &chromium).is_err());
        assert!(browser_auth_arguments("zen", "abc123.default-release", &zen).is_err());
        assert!(browser_auth_arguments("zen", "../chromium", &zen).is_err());

        fs::write(
            chromium.join("Default").join("Network").join("Cookies"),
            b"",
        )
        .unwrap();
        fs::write(
            zen.join("abc123.default-release").join("cookies.sqlite"),
            b"",
        )
        .unwrap();
        for (method, profile, directory, prefix) in [
            ("chrome", "Default", &chromium, "chrome:"),
            ("vivaldi", "Default", &chromium, "vivaldi:"),
            ("zen", "abc123.default-release", &zen, "firefox:"),
        ] {
            let args = browser_auth_arguments(method, profile, directory).unwrap();
            assert_eq!(args[0], "--cookies-from-browser");
            assert_eq!(
                args[1],
                std::ffi::OsString::from(format!("{prefix}{}", directory.join(profile).display()))
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn youtube_authentication_errors_explain_decryption_without_exposing_diagnostics() {
        use super::{YouTubeAuthentication as Auth, youtube_request_error_for};
        use crate::i18n::tr;

        let auth = Auth::Thorium {
            profile: "Default".into(),
        };
        let private_diagnostic =
            b"WARNING: Failed to decrypt with DPAPI: private-profile-path\nERROR: failed to load cookies";
        let error = youtube_request_error_for(&auth, "metadata", private_diagnostic);
        assert!(error.contains(tr("msg.youtube_cookie_decryption_error")));
        assert!(!error.contains("private-profile-path"));

        let error =
            youtube_request_error_for(&auth, "metadata", b"Sign in to confirm you are not a bot");
        assert!(error.contains(tr("msg.youtube_login_error")));

        let error = youtube_request_error_for(&auth, "metadata", b"HTTP Error 403: Forbidden");
        assert!(error.contains(tr("msg.youtube_http_error")));

        let error =
            youtube_request_error_for(&auth, "metadata", b"Unknown failure with private-token");
        assert!(error.contains(tr("msg.youtube_auth_request_failed")));
        assert!(!error.contains("private-token"));

        let no_auth = youtube_request_error_for(&Auth::None, "metadata", b"network error");
        assert!(no_auth.contains("network error"));
    }
}
