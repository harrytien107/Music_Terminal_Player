use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

pub(crate) const SEEK_SECONDS: u64 = 10;
pub(crate) const LIBRARY_FILE: &str = ".music-terminal-library";
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "aac", "aiff", "alac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "webm",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayerExit {
    Back,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopMode {
    Off,
    All,
    One,
}

impl LoopMode {
    pub(crate) fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

pub(crate) fn normalize_channel(channel: &str) -> String {
    channel
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches('@')
        .to_string()
}

pub(crate) fn is_supported_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
pub(crate) fn safe_file_name(name: &str) -> String {
    let mut safe = String::with_capacity(name.len());
    for ch in name.chars() {
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control() {
            safe.push('_');
        } else {
            safe.push(ch);
        }
    }
    let safe = safe.trim().trim_matches('.');
    if safe.is_empty() {
        "track.bin".to_string()
    } else {
        safe.to_string()
    }
}

pub(crate) fn clear_screen() -> Result<()> {
    execute!(
        io::stdout(),
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    Ok(())
}

pub(crate) fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

pub(crate) fn select_menu(title: &str, items: &[String]) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let _raw = RawMode::new()?;
    let mut selected = 0usize;
    let mut stdout = io::stdout();
    loop {
        let mut frame = format!("{title}\r\n\r\n");
        for (index, item) in items.iter().enumerate() {
            frame.push_str(&format!(
                "{} {}\r\n",
                if index == selected { ">" } else { " " },
                item
            ));
        }
        frame.push_str("\r\nUp/Down select | Enter confirm | Esc back");
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(items.len() - 1),
            KeyCode::Down => selected = (selected + 1) % items.len(),
            KeyCode::Enter => return Ok(Some(selected)),
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn progress_bar(elapsed: Duration, total: Option<Duration>, width: usize) -> String {
    let filled = total
        .filter(|duration| !duration.is_zero())
        .map(|duration| {
            ((elapsed.as_secs_f64() / duration.as_secs_f64()) * width as f64)
                .round()
                .clamp(0.0, width as f64) as usize
        })
        .unwrap_or(0);
    format!("{}{}", "/".repeat(filled), " ".repeat(width - filled))
}

pub(crate) fn fit_text(text: &str, width: usize) -> String {
    let mut value: String = text.chars().take(width).collect();
    let used = value.chars().count();
    if text.chars().count() > width && width > 0 {
        value.pop();
        value.push('…');
    }
    value.push_str(&" ".repeat(width.saturating_sub(used)));
    value
}

pub(crate) fn draw_frame(stdout: &mut io::Stdout, frame: &str) -> Result<()> {
    let mut output = Vec::new();
    queue!(output, cursor::MoveTo(0, 0))?;
    for (index, line) in frame.lines().enumerate() {
        if index > 0 {
            queue!(output, cursor::MoveToNextLine(1))?;
        }
        queue!(
            output,
            terminal::Clear(ClearType::CurrentLine),
            cursor::MoveToColumn(0)
        )?;
        output.write_all(line.trim_end_matches('\r').as_bytes())?;
    }
    queue!(output, terminal::Clear(ClearType::FromCursorDown))?;
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

pub(crate) fn draw_panel(stdout: &mut io::Stdout, title: &str, rows: &[String]) -> Result<()> {
    let width = usize::from(terminal::size().map(|size| size.0).unwrap_or(80))
        .saturating_sub(4)
        .clamp(28, 84);
    let mut frame = String::new();
    frame.push_str(&format!("┌{}┐\r\n", "─".repeat(width + 2)));
    frame.push_str(&format!("│ {} │\r\n", fit_text(title, width)));
    frame.push_str(&format!("├{}┤\r\n", "─".repeat(width + 2)));
    for row in rows {
        frame.push_str(&format!("│ {} │\r\n", fit_text(row, width)));
    }
    frame.push_str(&format!("└{}┘", "─".repeat(width + 2)));

    draw_frame(stdout, &frame)
}

pub(crate) fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub(crate) fn min_duration(a: Duration, b: Duration) -> Duration {
    if a <= b { a } else { b }
}

pub(crate) fn shuffle_slice<T>(items: &mut [T]) {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for index in (1..items.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        items.swap(index, (state as usize) % (index + 1));
    }
}

pub(crate) struct RawMode;

impl RawMode {
    pub(crate) fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}
