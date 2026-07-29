use std::fs;
use std::io::{self, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, ClearType};
use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};
use serde_json::Value;

use crate::util::{
    DATA_DIR, LoopMode, PlayerExit, RawMode, clear_screen, draw_panel, format_duration,
    insert_queue_next, load_volume, manage_queue_with_adder, playback_controls, progress_bar,
    prompt, save_volume, select_menu, shuffle_slice,
};

const TOOLS_FILE: &str = ".music-terminal/youtube-tools.txt";
const AUDIO_CHANNELS: u16 = 2;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const SEEK_STEP: Duration = Duration::from_secs(10);
const MIN_PLAYBACK_RATE: f32 = 0.5;
const MAX_PLAYBACK_RATE: f32 = 2.0;
const PLAYBACK_RATE_STEP: f32 = 0.25;
const SPONSOR_CHAPTER_TITLE: &str = "__MTP_SPONSOR__";
const YT_DLP_DOWNLOAD_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const FFMPEG_DOWNLOAD_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SponsorSegment {
    start: Duration,
    end: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct YouTubeTrack {
    title: String,
    webpage_url: String,
    duration: Option<Duration>,
    sponsor_segments: Option<Vec<SponsorSegment>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct YouTubeTools {
    pub(crate) yt_dlp: String,
    pub(crate) ffmpeg: String,
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
    offset: Duration,
    rate: f32,
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
            "▶  Play YouTube URL or playlist".to_string(),
            "⚙  YouTube tools and updater".to_string(),
            "←  Back".to_string(),
        ];
        match select_menu("YouTube audio", &items)? {
            Some(0) => {
                let tools = load_or_prompt_tools()?;
                let tracks = prompt_and_resolve_tracks(&tools)?;
                if tracks.is_empty() {
                    continue;
                }
                if play_youtube_tracks(tracks, &tools)? == PlayerExit::Quit {
                    return Ok(PlayerExit::Quit);
                }
            }
            Some(1) => manage_youtube_tools()?,
            Some(2) | None => return Ok(PlayerExit::Back),
            _ => unreachable!(),
        }
    }
}

fn load_or_prompt_tools() -> Result<YouTubeTools> {
    if let Ok(text) = fs::read_to_string(TOOLS_FILE) {
        if let Some(tools) = parse_youtube_tools(&text) {
            if validate_tool(&tools.yt_dlp, "--version").is_ok()
                && validate_tool(&tools.ffmpeg, "-version").is_ok()
            {
                return Ok(tools);
            }
        }
    }

    let items = vec![
        "↓  Download portable yt-dlp and FFmpeg".to_string(),
        "⌕  Use existing yt-dlp and FFmpeg installations".to_string(),
        "←  Back".to_string(),
    ];
    let tools = match select_menu("YouTube direct-stream setup", &items)? {
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
    println!("Enter existing executable paths. Command names on PATH also work.\n");
    let tools = YouTubeTools {
        yt_dlp: normalize_executable_input(&prompt("yt-dlp executable path: ")?),
        ffmpeg: normalize_executable_input(&prompt("FFmpeg executable path: ")?),
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
    println!("Downloading portable yt-dlp...");
    download_file(YT_DLP_DOWNLOAD_URL, &yt_dlp)?;
    println!("Downloading portable FFmpeg (this is a large download)...");
    download_file(FFMPEG_DOWNLOAD_URL, &archive)?;
    if extracted.exists() {
        fs::remove_dir_all(&extracted)?;
    }
    fs::create_dir_all(&extracted)?;
    println!("Extracting FFmpeg...");
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

fn manage_youtube_tools() -> Result<()> {
    let tools = load_or_prompt_tools()?;
    loop {
        let executable =
            std::env::current_exe().context("failed to locate the player executable")?;
        let portable = is_portable_yt_dlp(&tools.yt_dlp, &executable);
        let title = format!(
            "YouTube tools\nyt-dlp: {}\nFFmpeg: {}\nUpdater: {}",
            tool_version(&tools.yt_dlp, "--version"),
            tool_version(&tools.ffmpeg, "-version"),
            if portable {
                "portable stable channel"
            } else {
                "disabled for external/PATH installation"
            }
        );
        let items = vec![
            "↓  Update portable yt-dlp (stable)".to_string(),
            "←  Back".to_string(),
        ];
        match select_menu(&title, &items)? {
            Some(0) if portable => {
                clear_screen()?;
                println!("Updating portable yt-dlp on the stable channel...");
                let status = Command::new(&tools.yt_dlp)
                    .args(["--update-to", "stable"])
                    .status()
                    .context("failed to start the yt-dlp updater")?;
                if !status.success() {
                    bail!("yt-dlp stable update failed");
                }
                prompt("Press Enter to go back...")?;
            }
            Some(0) => {
                clear_screen()?;
                println!(
                    "This updater only manages the application-relative tools\\yt-dlp.exe.\nConfigured yt-dlp: {}",
                    tools.yt_dlp
                );
                prompt("Press Enter to go back...")?;
            }
            Some(1) | None => return Ok(()),
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
    clear_screen()?;
    println!(
        "YouTube audio\nTutor: Paste one URL, multiple space-separated URLs (https://... https://), or a playlist URL."
    );
    let input = prompt("URL(s): ")?;
    let urls = parse_youtube_urls(&input)?;
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    resolve_tracks(tools, &urls)
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
    println!("Resolving YouTube video and playlist metadata...");
    let output = Command::new(&tools.yt_dlp)
        .args([
            "--no-warnings",
            "--ignore-errors",
            "--yes-playlist",
            "--flat-playlist",
            "--print",
            "%(webpage_url)s\t%(title)S\t%(duration)s",
        ])
        .args(urls)
        .stdin(Stdio::null())
        .output()
        .context("failed to run yt-dlp")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("yt-dlp failed to resolve YouTube input: {}", error.trim());
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
    let mut fields = line.splitn(3, '\t');
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
    if webpage_url.is_empty() || title.is_empty() {
        return None;
    }
    Some(YouTubeTrack {
        title: title.to_string(),
        webpage_url: webpage_url.to_string(),
        duration,
        sponsor_segments: None,
    })
}

fn youtube_stream_format() -> &'static str {
    // ponytail: Progressive MP4 is less bandwidth-efficient than DASH, but its front-loaded index
    // makes FFmpeg seeking reliable. Add a local segment cache if format 18 disappears broadly.
    "18/bestaudio/best"
}

fn resolve_audio_url(tools: &YouTubeTools, track: &YouTubeTrack) -> Result<String> {
    let output = Command::new(&tools.yt_dlp)
        .args([
            "--no-warnings",
            "--no-playlist",
            "--format",
            youtube_stream_format(),
            "--get-url",
        ])
        .arg(&track.webpage_url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to resolve {}", track.title))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("yt-dlp failed to resolve audio stream: {}", error.trim());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
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
    playback_from_url(
        stream,
        tools,
        resolve_audio_url(tools, track)?,
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
    let Ok(Value::Array(chapters)) = serde_json::from_str::<Value>(text.trim()) else {
        return Vec::new();
    };
    let mut segments: Vec<_> = chapters
        .into_iter()
        .filter(|chapter| {
            chapter.get("title").and_then(Value::as_str) == Some(SPONSOR_CHAPTER_TITLE)
        })
        .filter_map(|chapter| {
            let start = chapter.get("start_time")?.as_f64()?;
            let end = chapter.get("end_time")?.as_f64()?;
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
    tools: &YouTubeTools,
    track: &YouTubeTrack,
) -> Result<Vec<SponsorSegment>> {
    let output = Command::new(&tools.yt_dlp)
        .args([
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            "--sponsorblock-mark",
            "sponsor",
            "--sponsorblock-chapter-title",
            SPONSOR_CHAPTER_TITLE,
            "--print",
            "%(chapters)j",
        ])
        .arg(&track.webpage_url)
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

fn cache_sponsor_segments(tools: &YouTubeTools, track: &mut YouTubeTrack) {
    if track.sponsor_segments.is_none() {
        track.sponsor_segments = Some(resolve_sponsor_segments(tools, track).unwrap_or_default());
    }
}

fn play_youtube_tracks(mut tracks: Vec<YouTubeTrack>, tools: &YouTubeTools) -> Result<PlayerExit> {
    let mut raw = Some(RawMode::new()?);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    let stream = OutputStreamBuilder::open_default_stream()
        .context("failed to open default audio output")?;
    let mut original_tracks = tracks.clone();
    let mut index = 0usize;
    let mut play_next = 0usize;
    let mut volume = load_volume();
    let mut shuffle = false;
    let mut loop_mode = LoopMode::Off;
    let mut rate = 1.0;
    let mut sponsor_enabled = true;
    let mut last_skipped = None;
    cache_sponsor_segments(tools, &mut tracks[index]);
    let mut playback = start_track(&stream, tools, &tracks[index], volume, rate)?;

    loop {
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
                restart_playback(&mut playback, &stream, tools, volume, segment.end, rate)?;
                last_skipped = Some(segment);
                continue;
            }
        }

        draw_youtube_player(
            &mut stdout,
            &tracks,
            index,
            &playback,
            volume,
            shuffle,
            loop_mode,
            sponsor_enabled,
        )?;

        if playback.empty() {
            if play_next > 0 && loop_mode != LoopMode::One {
                play_next -= 1;
            }
            match loop_mode {
                LoopMode::One => {}
                LoopMode::All => index = (index + 1) % tracks.len(),
                LoopMode::Off if index + 1 < tracks.len() => index += 1,
                LoopMode::Off => return Ok(PlayerExit::Back),
            }
            cache_sponsor_segments(tools, &mut tracks[index]);
            last_skipped = None;
            replace_track(&mut playback, &stream, tools, &tracks[index], volume, rate)?;
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
            return Ok(PlayerExit::Quit);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(PlayerExit::Quit),
            KeyCode::Char('b') => return Ok(PlayerExit::Back),
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                if playback.is_paused() {
                    playback.play();
                } else {
                    playback.pause();
                }
            }
            KeyCode::Char('n') => {
                if play_next > 0 {
                    play_next -= 1;
                }
                index = (index + 1) % tracks.len();
                cache_sponsor_segments(tools, &mut tracks[index]);
                last_skipped = None;
                replace_track(&mut playback, &stream, tools, &tracks[index], volume, rate)?;
            }
            KeyCode::Char('v') => {
                index = if index == 0 {
                    tracks.len() - 1
                } else {
                    index - 1
                };
                cache_sponsor_segments(tools, &mut tracks[index]);
                last_skipped = None;
                replace_track(&mut playback, &stream, tools, &tracks[index], volume, rate)?;
            }
            KeyCode::Left | KeyCode::Right => {
                let target = seek_target(
                    playback.elapsed(tracks[index].duration),
                    tracks[index].duration,
                    key.code == KeyCode::Right,
                );
                last_skipped = None;
                restart_playback(&mut playback, &stream, tools, volume, target, rate)?;
            }
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.10).min(1.5);
                playback.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Down | KeyCode::Char('-') => {
                volume = (volume - 0.10).max(0.0);
                playback.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Char(',') | KeyCode::Char('.') => {
                let next_rate = stepped_rate(rate, key.code == KeyCode::Char('.'));
                if next_rate != rate {
                    let elapsed = playback.elapsed(tracks[index].duration);
                    rate = next_rate;
                    restart_playback(&mut playback, &stream, tools, volume, elapsed, rate)?;
                }
            }
            KeyCode::Char('s') => sponsor_enabled = !sponsor_enabled,
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
            KeyCode::Char('u') => {
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
                    if play_next > 0 && loop_mode != LoopMode::One {
                        play_next -= 1;
                    }
                    match loop_mode {
                        LoopMode::One => {}
                        LoopMode::All => index = (index + 1) % tracks.len(),
                        LoopMode::Off if index + 1 < tracks.len() => index += 1,
                        LoopMode::Off => return Ok(PlayerExit::Back),
                    }
                    cache_sponsor_segments(tools, &mut tracks[index]);
                    last_skipped = None;
                    replace_track(&mut playback, &stream, tools, &tracks[index], volume, rate)?;
                } else if edit.restart {
                    play_next = 0;
                    cache_sponsor_segments(tools, &mut tracks[index]);
                    last_skipped = None;
                    replace_track(&mut playback, &stream, tools, &tracks[index], volume, rate)?;
                }
                if edit.changed {
                    shuffle = false;
                }
            }
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
    shuffle: bool,
    loop_mode: LoopMode,
    sponsor_enabled: bool,
) -> Result<()> {
    let state = if playback.is_paused() {
        "paused"
    } else {
        "playing"
    };
    let duration = tracks[index].duration;
    let elapsed = playback.elapsed(duration);
    let total = duration
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let mut rows = vec![
        format!(
            "Track {}/{} | {}",
            index + 1,
            tracks.len(),
            tracks[index].title
        ),
        String::new(),
        format!(
            "[{}] {}/{}",
            progress_bar(elapsed, duration, 12),
            format_duration(elapsed),
            total
        ),
        format!(
            "{} · {:.0}% · {:.2}x · shuffle {} · loop {}",
            state,
            volume * 100.0,
            playback.rate,
            if shuffle { "on" } else { "off" },
            loop_mode.label(),
        ),
        format!(
            "SponsorBlock {} · [a] add next URL or playlist",
            if sponsor_enabled { "on" } else { "off" }
        ),
    ];
    rows.extend(youtube_playback_controls());
    draw_panel(stdout, "Music Terminal Player · YouTube", &rows)
}

fn youtube_playback_controls() -> Vec<String> {
    let mut rows = playback_controls();
    rows.pop();
    rows.extend([
        "├────────────────┼────────────────┼────────────────┤".to_string(),
        "│ [←/→] seek 10s │ [,/.] ±0.25x   │ [s] sponsors   │".to_string(),
        "└────────────────┴────────────────┴────────────────┘".to_string(),
    ]);
    rows
}

#[cfg(test)]
mod youtube_tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{
        SPONSOR_CHAPTER_TITLE, SponsorSegment, is_portable_yt_dlp, logical_elapsed,
        parse_sponsor_segments, parse_youtube_track_line, seek_target, sponsor_skip_target,
        stepped_rate, youtube_playback_controls, youtube_stream_format,
    };

    #[test]
    fn metadata_line_parses_duration_and_missing_duration() {
        let track = parse_youtube_track_line("https://youtu.be/abc\tExample title\t123.5").unwrap();
        assert_eq!(track.title, "Example title");
        assert_eq!(track.duration.unwrap().as_secs_f64(), 123.5);

        let live = parse_youtube_track_line("https://youtu.be/live\tLive stream\tNA").unwrap();
        assert_eq!(live.duration, None);
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
    fn youtube_stream_prefers_seekable_progressive_mp4() {
        assert_eq!(youtube_stream_format(), "18/bestaudio/best");
    }

    #[test]
    fn sponsor_segments_parse_and_do_not_repeat() {
        let json = format!(
            r#"[{{"title":"ignored","start_time":1,"end_time":2}},{{"title":"{SPONSOR_CHAPTER_TITLE}","start_time":10.5,"end_time":20}}]"#
        );
        let segments = parse_sponsor_segments(&json);
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
    fn updater_only_accepts_executable_relative_portable_yt_dlp() {
        let executable = Path::new(r"C:\Player\music-terminal-player.exe");
        assert!(is_portable_yt_dlp(
            r"C:\Player\tools\yt-dlp.exe",
            executable
        ));
        assert!(!is_portable_yt_dlp("yt-dlp", executable));
        assert!(!is_portable_yt_dlp(r"C:\Other\yt-dlp.exe", executable));
    }

    #[test]
    fn youtube_controls_add_seek_speed_and_sponsor_row() {
        let controls = youtube_playback_controls();
        assert!(controls.iter().any(|row| row.contains("[←/→] seek 10s")));
        assert!(controls.iter().any(|row| row.contains("[,/.] ±0.25x")));
        assert!(controls.iter().any(|row| row.contains("[s] sponsors")));
        assert_eq!(
            controls.last().unwrap(),
            "└────────────────┴────────────────┴────────────────┘"
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
}
