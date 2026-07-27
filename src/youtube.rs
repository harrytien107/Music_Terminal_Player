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

use crate::util::{
    DATA_DIR, LoopMode, PlayerExit, RawMode, clear_screen, draw_panel, format_duration,
    insert_queue_next, load_volume, manage_queue_with_adder, playback_controls, progress_bar,
    prompt, save_volume, select_menu, shuffle_slice,
};

const TOOLS_FILE: &str = ".music-terminal/youtube-tools.txt";
const AUDIO_CHANNELS: u16 = 2;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const YT_DLP_DOWNLOAD_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const FFMPEG_DOWNLOAD_URL: &str =
    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct YouTubeTrack {
    title: String,
    webpage_url: String,
    duration: Option<Duration>,
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
}

impl YouTubePlayback {
    fn stop(&self) {
        if let Ok(mut child) = self.ffmpeg.lock() {
            let _ = child.kill();
        }
        self.sink.stop();
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

pub(crate) fn play_youtube() -> Result<PlayerExit> {
    let tools = load_or_prompt_tools()?;
    let tracks = prompt_and_resolve_tracks(&tools)?;
    if tracks.is_empty() {
        bail!("no playable YouTube tracks were found");
    }
    play_youtube_tracks(tracks, &tools)
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
    })
}

fn resolve_audio_url(tools: &YouTubeTools, track: &YouTubeTrack) -> Result<String> {
    let output = Command::new(&tools.yt_dlp)
        .args([
            "--no-warnings",
            "--no-playlist",
            "--format",
            "bestaudio/best",
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

fn spawn_ffmpeg_source(ffmpeg: &str, media_url: &str) -> Result<(FfmpegPcmSource, SharedChild)> {
    let mut child = Command::new(ffmpeg)
        .args(["-nostdin", "-loglevel", "error"])
        .arg("-i")
        .arg(media_url)
        .args([
            "-vn",
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

fn start_track(
    stream: &OutputStream,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
    volume: f32,
) -> Result<YouTubePlayback> {
    let media_url = resolve_audio_url(tools, track)?;
    let (source, ffmpeg) = spawn_ffmpeg_source(&tools.ffmpeg, &media_url)?;
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    sink.append(source);
    Ok(YouTubePlayback { sink, ffmpeg })
}

fn replace_track(
    playback: &mut YouTubePlayback,
    stream: &OutputStream,
    tools: &YouTubeTools,
    track: &YouTubeTrack,
    volume: f32,
) -> Result<()> {
    let replacement = start_track(stream, tools, track, volume)?;
    playback.stop();
    *playback = replacement;
    Ok(())
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
    let mut sink = start_track(&stream, tools, &tracks[index], volume)?;

    loop {
        draw_youtube_player(
            &mut stdout,
            &tracks,
            index,
            &sink,
            volume,
            shuffle,
            loop_mode,
        )?;

        if sink.empty() {
            if play_next > 0 && loop_mode != LoopMode::One {
                play_next -= 1;
            }
            match loop_mode {
                LoopMode::One => {}
                LoopMode::All => index = (index + 1) % tracks.len(),
                LoopMode::Off if index + 1 < tracks.len() => index += 1,
                LoopMode::Off => return Ok(PlayerExit::Back),
            }
            replace_track(&mut sink, &stream, tools, &tracks[index], volume)?;
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
                if sink.is_paused() {
                    sink.play();
                } else {
                    sink.pause();
                }
            }
            KeyCode::Char('n') => {
                if play_next > 0 {
                    play_next -= 1;
                }
                index = (index + 1) % tracks.len();
                replace_track(&mut sink, &stream, tools, &tracks[index], volume)?;
            }
            KeyCode::Char('v') => {
                index = if index == 0 {
                    tracks.len() - 1
                } else {
                    index - 1
                };
                replace_track(&mut sink, &stream, tools, &tracks[index], volume)?;
            }
            KeyCode::Left => {
                volume = (volume - 0.01).max(0.0);
                sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Right => {
                volume = (volume + 0.01).min(1.5);
                sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.10).min(1.5);
                sink.set_volume(volume);
                save_volume(volume)?;
            }
            KeyCode::Down | KeyCode::Char('-') => {
                volume = (volume - 0.10).max(0.0);
                sink.set_volume(volume);
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
                    || sink.empty(),
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
                    replace_track(&mut sink, &stream, tools, &tracks[index], volume)?;
                } else if edit.restart {
                    play_next = 0;
                    replace_track(&mut sink, &stream, tools, &tracks[index], volume)?;
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
    sink: &Sink,
    volume: f32,
    shuffle: bool,
    loop_mode: LoopMode,
) -> Result<()> {
    let state = if sink.is_paused() {
        "paused"
    } else {
        "playing"
    };
    let elapsed = sink.get_pos();
    let duration = tracks[index].duration;
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
            "{} · {:.0}% · shuffle {} · loop {}",
            state,
            volume * 100.0,
            if shuffle { "on" } else { "off" },
            loop_mode.label()
        ),
        "[a] add next YouTube URL or playlist".to_string(),
    ];
    rows.extend(playback_controls());
    draw_panel(stdout, "Music Terminal Player · YouTube", &rows)
}

#[cfg(test)]
mod youtube_tests {
    use super::parse_youtube_track_line;

    #[test]
    fn metadata_line_parses_duration_and_missing_duration() {
        let track = parse_youtube_track_line("https://youtu.be/abc\tExample title\t123.5").unwrap();
        assert_eq!(track.title, "Example title");
        assert_eq!(track.duration.unwrap().as_secs_f64(), 123.5);

        let live = parse_youtube_track_line("https://youtu.be/live\tLive stream\tNA").unwrap();
        assert_eq!(live.duration, None);
    }
}
