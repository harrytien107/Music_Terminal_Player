use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self, ClearType};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};

use crate::util::{
    LoopMode, PlayerExit, RawMode, SEEK_SECONDS, draw_frame, draw_panel, format_duration,
    is_supported_audio_path, min_duration, progress_bar, shuffle_slice,
};
const TRACK_LIST_PAGE_SIZE: usize = 12;

pub(crate) fn play_path(path: &Path) -> Result<PlayerExit> {
    let tracks = collect_tracks(path)?;
    if tracks.is_empty() {
        bail!("no supported audio files found in {}", path.display());
    }
    play_tracks(tracks)
}

pub(crate) fn play_tracks(mut tracks: Vec<PathBuf>) -> Result<PlayerExit> {
    tracks.retain(|track| track.is_file() && is_supported_audio_path(track));
    if tracks.is_empty() {
        bail!("playlist has no available local songs");
    }

    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    let stream = OutputStreamBuilder::open_default_stream()
        .context("failed to open default audio output")?;
    let original_tracks = tracks.clone();
    let mut index = 0usize;
    let mut volume = 0.8f32;
    let mut shuffle = false;
    let mut loop_mode = LoopMode::Off;
    let mut duration;
    let mut sink;
    (sink, duration) = start_track(&stream, &tracks[index], volume, false)?;

    loop {
        draw_player(
            &mut stdout,
            &tracks,
            index,
            duration,
            &sink,
            volume,
            shuffle,
            loop_mode,
        )?;

        if sink.empty() {
            match loop_mode {
                LoopMode::One => {}
                LoopMode::All => index = (index + 1) % tracks.len(),
                LoopMode::Off if index + 1 < tracks.len() => index += 1,
                LoopMode::Off => return Ok(PlayerExit::Back),
            }
            (sink, duration) = start_track(&stream, &tracks[index], volume, false)?;
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
            KeyCode::Char('q') | KeyCode::Esc => return Ok(PlayerExit::Quit),
            KeyCode::Char('b') => return Ok(PlayerExit::Back),
            KeyCode::Char('p') => {
                if sink.is_paused() {
                    sink.play();
                } else {
                    sink.pause();
                }
            }
            KeyCode::Char('n') => {
                index = (index + 1) % tracks.len();
                (sink, duration) = start_track(&stream, &tracks[index], volume, false)?;
            }
            KeyCode::Char('v') => {
                index = if index == 0 {
                    tracks.len() - 1
                } else {
                    index - 1
                };
                (sink, duration) = start_track(&stream, &tracks[index], volume, false)?;
            }
            KeyCode::Left => {
                let target = sink
                    .get_pos()
                    .saturating_sub(Duration::from_secs(SEEK_SECONDS));
                let _ = sink.try_seek(target);
            }
            KeyCode::Right => {
                let now = sink.get_pos();
                let mut target = now + Duration::from_secs(SEEK_SECONDS);
                if let Some(total) = duration {
                    target = min_duration(target, total);
                }
                let _ = sink.try_seek(target);
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
            KeyCode::Char('+') | KeyCode::Char('=') => {
                volume = (volume + 0.05).min(1.5);
                sink.set_volume(volume);
            }
            KeyCode::Char('-') => {
                volume = (volume - 0.05).max(0.0);
                sink.set_volume(volume);
            }
            _ => {}
        }
    }
}

fn start_track(
    stream: &OutputStream,
    path: &Path,
    volume: f32,
    paused: bool,
) -> Result<(Sink, Option<Duration>)> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let decoder = Decoder::try_from(BufReader::new(file))
        .with_context(|| format!("failed to decode {}", path.display()))?;
    let duration = decoder.total_duration();
    let sink = Sink::connect_new(stream.mixer());
    sink.set_volume(volume);
    sink.append(decoder);
    if paused {
        sink.pause();
    }
    Ok((sink, duration))
}

fn draw_player(
    stdout: &mut io::Stdout,
    tracks: &[PathBuf],
    index: usize,
    duration: Option<Duration>,
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
    let total = duration
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let rows = vec![
        format!(
            "Track {}/{} | {}",
            index + 1,
            tracks.len(),
            tracks[index].display()
        ),
        String::new(),
        format!(
            "[{}] {}/{}",
            progress_bar(elapsed, duration, 12),
            format_duration(elapsed),
            total
        ),
        format!(
            "{} | volume {:.0}% | shuffle {} | loop {}",
            state,
            volume * 100.0,
            if shuffle { "on" } else { "off" },
            loop_mode.label()
        ),
        String::new(),
        "[p] play/pause | [v] previous | [n] next | [r] shuffle | [l] loop".to_string(),
        "[Left/Right] seek | [+/-] volume | [b] menu | [q] quit".to_string(),
    ];
    draw_panel(stdout, "Music Terminal Player · Local", &rows)
}

pub(crate) fn collect_tracks(path: &Path) -> Result<Vec<PathBuf>> {
    let mut tracks = Vec::new();
    if path.is_file() {
        if is_supported_audio_path(path) {
            tracks.push(path.to_path_buf());
        }
    } else if path.is_dir() {
        collect_tracks_recursive(path, &mut tracks)?;
    } else {
        bail!("path does not exist: {}", path.display());
    }
    tracks.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    Ok(tracks)
}

pub(crate) fn choose_local_tracks(
    tracks: &[PathBuf],
    selected_paths: &[PathBuf],
) -> Result<Option<Vec<PathBuf>>> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut query = String::new();
    let mut matches: Vec<_> = (0..tracks.len()).collect();
    let mut selected_row = 0usize;
    let mut selected: HashSet<PathBuf> = selected_paths.iter().cloned().collect();

    loop {
        selected_row = if matches.is_empty() {
            0
        } else {
            selected_row.min(matches.len() - 1)
        };
        let start = selected_row
            .saturating_sub(TRACK_LIST_PAGE_SIZE / 2)
            .min(matches.len().saturating_sub(TRACK_LIST_PAGE_SIZE));
        let end = (start + TRACK_LIST_PAGE_SIZE).min(matches.len());
        let mut frame = format!(
            "Choose local playlist songs\r\nSearch: {query}_ | {} selected | {} matches\r\n\r\n",
            selected.len(),
            matches.len()
        );
        for (offset, &track_index) in matches[start..end].iter().enumerate() {
            let track = &tracks[track_index];
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if start + offset == selected_row {
                    ">"
                } else {
                    " "
                },
                if selected.contains(track) { "x" } else { " " },
                track.display()
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
                let path = &tracks[matches[selected_row]];
                if !selected.remove(path) {
                    selected.insert(path.clone());
                }
            }
            KeyCode::Backspace => {
                query.pop();
                matches = search_local_tracks(tracks, &query);
                selected_row = 0;
            }
            KeyCode::Char(character) if !character.is_control() => {
                query.push(character);
                matches = search_local_tracks(tracks, &query);
                selected_row = 0;
            }
            KeyCode::Enter => {
                return Ok(Some(
                    tracks
                        .iter()
                        .filter(|track| selected.contains(*track))
                        .cloned()
                        .collect(),
                ));
            }
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn search_local_tracks(tracks: &[PathBuf], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.to_string_lossy().to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}

fn collect_tracks_recursive(path: &Path, tracks: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_tracks_recursive(&path, tracks)?;
        } else if path.is_file() && is_supported_audio_path(&path) {
            tracks.push(path);
        }
    }
    Ok(())
}
