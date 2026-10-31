use std::collections::HashSet;
use std::fs;
use std::hash::Hash;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use anyhow::Result;

use crate::i18n::tr;
use crate::lyrics::Lyrics;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

const BOLD_START: char = '\u{1}';
const BOLD_END: char = '\u{2}';
const COLOR_WHITE: char = '\u{3}';
const COLOR_RED: char = '\u{4}';
const COLOR_GREEN: char = '\u{5}';
const COLOR_YELLOW: char = '\u{6}';
const COLOR_BLUE: char = '\u{7}';
const COLOR_MAGENTA: char = '\u{8}';
const COLOR_CYAN: char = '\u{b}';
const COLOR_DEFAULT: char = '\u{c}';

pub(crate) const NORMAL_TEXT_COLOR: Color = Color::Rgb {
    r: 0xd3,
    g: 0xc6,
    b: 0xaa,
};

pub(crate) const DATA_DIR: &str = ".music-terminal";
pub(crate) const LIBRARY_FILE: &str = ".music-terminal/library.txt";
pub(crate) const PLAYLIST_FILE: &str = ".music-terminal/playlists.txt";
pub(crate) const SETTINGS_FILE: &str = ".music-terminal/settings.txt";
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "aac", "aiff", "alac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "webm",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayerExit {
    Back,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LyricColor {
    Default,
    White,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl LyricColor {
    const ALL: [Self; 8] = [
        Self::Default,
        Self::White,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::White => "white",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Blue => "blue",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
        }
    }

    fn from_key(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|color| color.key() == value)
            .unwrap_or(Self::Default)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Default => tr("msg.default_warm"),
            Self::White => tr("msg.white"),
            Self::Red => tr("msg.red"),
            Self::Green => tr("msg.green"),
            Self::Yellow => tr("msg.yellow"),
            Self::Blue => tr("msg.blue"),
            Self::Magenta => tr("msg.magenta"),
            Self::Cyan => tr("msg.cyan"),
        }
    }

    fn marker(self) -> char {
        match self {
            Self::Default => COLOR_DEFAULT,
            Self::White => COLOR_WHITE,
            Self::Red => COLOR_RED,
            Self::Green => COLOR_GREEN,
            Self::Yellow => COLOR_YELLOW,
            Self::Blue => COLOR_BLUE,
            Self::Magenta => COLOR_MAGENTA,
            Self::Cyan => COLOR_CYAN,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LyricColorTarget {
    Active,
    Inactive,
    Plain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptColorTarget {
    Active,
    Inactive,
}

impl TranscriptColorTarget {
    fn setting_key(self) -> &'static str {
        match self {
            Self::Active => "transcript.color.active",
            Self::Inactive => "transcript.color.inactive",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => tr("msg.active_transcript_color"),
            Self::Inactive => tr("msg.inactive_transcript_color"),
        }
    }
}

impl LyricColorTarget {
    fn setting_key(self) -> &'static str {
        match self {
            Self::Active => "lyrics.color.active",
            Self::Inactive => "lyrics.color.inactive",
            Self::Plain => "lyrics.color.plain",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => tr("msg.active_synchronized_lyric_color"),
            Self::Inactive => tr("msg.inactive_synchronized_lyric_color"),
            Self::Plain => tr("msg.plain_lyrics_color"),
        }
    }
}

pub(crate) enum LyricsEditorResult {
    Handled,
    Close,
    Visibility(bool),
    Pass,
}

pub(crate) struct LyricsSettingsEditor {
    selected: usize,
    color_target: Option<LyricColorTarget>,
    color_selected: usize,
}

pub(crate) enum TranscriptEditorResult {
    Handled,
    Close,
    Visibility(bool),
    LanguageChanged,
    Pass,
}

pub(crate) struct TranscriptSettingsEditor {
    selected: usize,
    color_target: Option<TranscriptColorTarget>,
    color_selected: usize,
}

impl TranscriptSettingsEditor {
    pub(crate) fn new() -> Self {
        Self {
            selected: 0,
            color_target: None,
            color_selected: 0,
        }
    }

    fn title(&self) -> &'static str {
        self.color_target
            .map(TranscriptColorTarget::label)
            .unwrap_or_else(|| tr("msg.transcript_settings"))
    }

    fn rows(&self, transcript_visible: bool) -> Vec<String> {
        if self.color_target.is_some() {
            let mut rows = LyricColor::ALL
                .into_iter()
                .enumerate()
                .map(|(index, color)| {
                    let label = color_text(color.label(), color);
                    format!(
                        "{} {label}",
                        if index == self.color_selected {
                            ">"
                        } else {
                            " "
                        }
                    )
                })
                .collect::<Vec<_>>();
            rows.push(format!(
                "{} ← {}",
                if self.color_selected == LyricColor::ALL.len() {
                    ">"
                } else {
                    " "
                },
                tr("msg.back")
            ));
            return rows;
        }

        let position = if load_setting_value("transcript.position").as_deref() == Some("below") {
            tr("msg.below_player")
        } else {
            tr("msg.next_to_player")
        };
        let alignment = if load_setting_value("transcript.alignment").as_deref() == Some("center") {
            tr("msg.center")
        } else {
            tr("msg.left")
        };
        let language = transcript_language_label(
            load_setting_value("transcript.language")
                .as_deref()
                .unwrap_or("auto"),
        );
        let active = transcript_color_setting(TranscriptColorTarget::Active);
        let inactive = transcript_color_setting(TranscriptColorTarget::Inactive);
        let items = [
            format!(
                "{}: {}  {}",
                tr("msg.transcript"),
                if transcript_visible {
                    tr("msg.shown")
                } else {
                    tr("msg.hidden")
                },
                tr("msg.y_show_hide_transcript")
            ),
            format!("{}: {position}", tr("msg.transcript_position")),
            format!("{}: {alignment}", tr("msg.transcript_alignment")),
            format!("{}: {language}", tr("msg.transcript_language")),
            format!(
                "{}: {}",
                tr("msg.active_transcript_color"),
                color_text(active.label(), active)
            ),
            format!(
                "{}: {}",
                tr("msg.inactive_transcript_color"),
                color_text(inactive.label(), inactive)
            ),
            format!("← {}", tr("msg.back")),
        ];
        items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                format!("{} {item}", if index == self.selected { ">" } else { " " })
            })
            .collect()
    }

    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        transcript_visible: bool,
    ) -> Result<TranscriptEditorResult> {
        if code == KeyCode::Char('y') && modifiers.is_empty() {
            let visible = !transcript_visible;
            save_setting_bool("playback.transcript", visible)?;
            return Ok(TranscriptEditorResult::Visibility(visible));
        }

        if let Some(target) = self.color_target {
            match code {
                KeyCode::Up => {
                    self.color_selected = self
                        .color_selected
                        .checked_sub(1)
                        .unwrap_or(LyricColor::ALL.len());
                }
                KeyCode::Down => {
                    self.color_selected = (self.color_selected + 1) % (LyricColor::ALL.len() + 1);
                }
                KeyCode::Enter if self.color_selected < LyricColor::ALL.len() => {
                    save_setting_value(
                        target.setting_key(),
                        LyricColor::ALL[self.color_selected].key(),
                    )?;
                    self.color_target = None;
                }
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('b') => {
                    self.color_target = None;
                }
                _ => return Ok(TranscriptEditorResult::Pass),
            }
            return Ok(TranscriptEditorResult::Handled);
        }

        match code {
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(6),
            KeyCode::Down => self.selected = (self.selected + 1) % 7,
            KeyCode::Enter => match self.selected {
                0 => {
                    let visible = !transcript_visible;
                    save_setting_bool("playback.transcript", visible)?;
                    return Ok(TranscriptEditorResult::Visibility(visible));
                }
                1 => {
                    let below =
                        load_setting_value("transcript.position").as_deref() == Some("below");
                    save_setting_value(
                        "transcript.position",
                        if below { "side" } else { "below" },
                    )?;
                }
                2 => {
                    let centered =
                        load_setting_value("transcript.alignment").as_deref() == Some("center");
                    save_setting_value(
                        "transcript.alignment",
                        if centered { "left" } else { "center" },
                    )?;
                }
                3 => {
                    let current = load_setting_value("transcript.language")
                        .unwrap_or_else(|| "auto".to_string());
                    save_setting_value("transcript.language", next_transcript_language(&current))?;
                    return Ok(TranscriptEditorResult::LanguageChanged);
                }
                4..=5 => {
                    let target = if self.selected == 4 {
                        TranscriptColorTarget::Active
                    } else {
                        TranscriptColorTarget::Inactive
                    };
                    let current = transcript_color_setting(target);
                    self.color_selected = LyricColor::ALL
                        .iter()
                        .position(|color| *color == current)
                        .unwrap_or(0);
                    self.color_target = Some(target);
                }
                6 => return Ok(TranscriptEditorResult::Close),
                _ => unreachable!(),
            },
            KeyCode::Esc | KeyCode::Char('b') => return Ok(TranscriptEditorResult::Close),
            _ => return Ok(TranscriptEditorResult::Pass),
        }
        Ok(TranscriptEditorResult::Handled)
    }
}

fn transcript_language_label(value: &str) -> &'static str {
    match value {
        "original" => tr("msg.original"),
        "en" => tr("msg.english"),
        "vi" => tr("msg.vietnamese"),
        "ja" => tr("msg.japanese"),
        _ => tr("msg.auto"),
    }
}

fn next_transcript_language(value: &str) -> &'static str {
    match value {
        "auto" => "original",
        "original" => "en",
        "en" => "vi",
        "vi" => "ja",
        _ => "auto",
    }
}

impl LyricsSettingsEditor {
    pub(crate) fn new() -> Self {
        Self {
            selected: 0,
            color_target: None,
            color_selected: 0,
        }
    }

    fn title(&self) -> &'static str {
        self.color_target
            .map(LyricColorTarget::label)
            .unwrap_or_else(|| tr("msg.lyrics_settings"))
    }

    fn rows(&self, lyrics_visible: bool) -> Vec<String> {
        if self.color_target.is_some() {
            let mut rows = LyricColor::ALL
                .into_iter()
                .enumerate()
                .map(|(index, color)| {
                    let label = color_text(color.label(), color);
                    format!(
                        "{} {label}",
                        if index == self.color_selected {
                            ">"
                        } else {
                            " "
                        }
                    )
                })
                .collect::<Vec<_>>();
            rows.push(format!(
                "{} ← {}",
                if self.color_selected == LyricColor::ALL.len() {
                    ">"
                } else {
                    " "
                },
                tr("msg.back")
            ));
            return rows;
        }

        let position = if load_setting_value("lyrics.position").as_deref() == Some("below") {
            tr("msg.below_player")
        } else {
            tr("msg.next_to_player")
        };
        let alignment = if load_setting_value("lyrics.alignment").as_deref() == Some("center") {
            tr("msg.center")
        } else {
            tr("msg.left")
        };
        let active = lyric_color_setting(LyricColorTarget::Active);
        let inactive = lyric_color_setting(LyricColorTarget::Inactive);
        let plain = lyric_color_setting(LyricColorTarget::Plain);
        let items = [
            format!(
                "{}: {}  {}",
                tr("msg.lyrics"),
                if lyrics_visible {
                    tr("msg.shown")
                } else {
                    tr("msg.hidden")
                },
                tr("msg.y_show_hide_lyrics")
            ),
            format!("{}: {position}", tr("msg.lyrics_position")),
            format!("{}: {alignment}", tr("msg.lyrics_alignment")),
            format!(
                "{}: {}",
                tr("msg.active_synchronized_lyric_color"),
                color_text(active.label(), active)
            ),
            format!(
                "{}: {}",
                tr("msg.inactive_synchronized_lyric_color"),
                color_text(inactive.label(), inactive)
            ),
            format!(
                "{}: {}",
                tr("msg.plain_lyrics_color"),
                color_text(plain.label(), plain)
            ),
            format!("← {}", tr("msg.back")),
        ];
        items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                format!("{} {item}", if index == self.selected { ">" } else { " " })
            })
            .collect()
    }

    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        lyrics_visible: bool,
    ) -> Result<LyricsEditorResult> {
        if code == KeyCode::Char('y') && modifiers.is_empty() {
            let visible = !lyrics_visible;
            save_setting_bool("playback.lyrics", visible)?;
            return Ok(LyricsEditorResult::Visibility(visible));
        }

        if let Some(target) = self.color_target {
            match code {
                KeyCode::Up => {
                    self.color_selected = self
                        .color_selected
                        .checked_sub(1)
                        .unwrap_or(LyricColor::ALL.len());
                }
                KeyCode::Down => {
                    self.color_selected = (self.color_selected + 1) % (LyricColor::ALL.len() + 1);
                }
                KeyCode::Enter if self.color_selected < LyricColor::ALL.len() => {
                    save_setting_value(
                        target.setting_key(),
                        LyricColor::ALL[self.color_selected].key(),
                    )?;
                    self.color_target = None;
                }
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('b') => {
                    self.color_target = None;
                }
                _ => return Ok(LyricsEditorResult::Pass),
            }
            return Ok(LyricsEditorResult::Handled);
        }

        match code {
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(6),
            KeyCode::Down => self.selected = (self.selected + 1) % 7,
            KeyCode::Enter => match self.selected {
                0 => {
                    let visible = !lyrics_visible;
                    save_setting_bool("playback.lyrics", visible)?;
                    return Ok(LyricsEditorResult::Visibility(visible));
                }
                1 => {
                    let below = load_setting_value("lyrics.position").as_deref() == Some("below");
                    save_setting_value("lyrics.position", if below { "side" } else { "below" })?;
                }
                2 => {
                    let centered =
                        load_setting_value("lyrics.alignment").as_deref() == Some("center");
                    save_setting_value(
                        "lyrics.alignment",
                        if centered { "left" } else { "center" },
                    )?;
                }
                3..=5 => {
                    let target = match self.selected {
                        3 => LyricColorTarget::Active,
                        4 => LyricColorTarget::Inactive,
                        _ => LyricColorTarget::Plain,
                    };
                    let current = lyric_color_setting(target);
                    self.color_selected = LyricColor::ALL
                        .iter()
                        .position(|color| *color == current)
                        .unwrap_or(0);
                    self.color_target = Some(target);
                }
                6 => return Ok(LyricsEditorResult::Close),
                _ => unreachable!(),
            },
            KeyCode::Esc | KeyCode::Char('b') => return Ok(LyricsEditorResult::Close),
            _ => return Ok(LyricsEditorResult::Pass),
        }
        Ok(LyricsEditorResult::Handled)
    }
}

pub(crate) struct QueueEdit {
    pub(crate) index: usize,
    pub(crate) restart: bool,
    pub(crate) finished: bool,
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
            Self::Off => tr("msg.off"),
            Self::All => tr("msg.all"),
            Self::One => tr("msg.one"),
        }
    }
}

pub(crate) fn prepare_data_dir() -> Result<()> {
    fs::create_dir_all(Path::new(DATA_DIR).join("catalogs"))?;
    migrate_file(".music-terminal-library", LIBRARY_FILE)?;
    migrate_file(".music-terminal-playlists", PLAYLIST_FILE)?;
    migrate_file(
        ".telegram.credentials",
        Path::new(DATA_DIR).join("telegram.credentials"),
    )?;
    migrate_file(
        ".telegram-cache",
        Path::new(DATA_DIR).join("telegram-cache"),
    )?;

    for entry in fs::read_dir(".")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".telegram.session") {
            migrate_file(
                entry.path(),
                Path::new(DATA_DIR).join(name.trim_start_matches('.')),
            )?;
        } else if let Some(channel) = name
            .strip_prefix(".telegram-")
            .and_then(|name| name.strip_suffix(".tracks.txt"))
        {
            migrate_file(
                entry.path(),
                Path::new(DATA_DIR)
                    .join("catalogs")
                    .join(format!("{channel}.tracks.txt")),
            )?;
        }
    }

    Ok(())
}

fn migrate_file(old: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<()> {
    let old = old.as_ref();
    let new = new.as_ref();
    if !old.exists() || new.exists() {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(old, new)?;
    Ok(())
}

pub(crate) const MAX_VOLUME: f32 = 1.0;

pub(crate) fn load_volume() -> f32 {
    fs::read_to_string(SETTINGS_FILE)
        .ok()
        .map(|text| parse_volume_settings(&text))
        .unwrap_or(0.8)
}

pub(crate) fn parse_volume_settings(text: &str) -> f32 {
    text.lines()
        .find_map(|line| line.strip_prefix("volume="))
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|volume| volume.is_finite())
        .unwrap_or(0.8)
        .clamp(0.0, MAX_VOLUME)
}

pub(crate) fn save_volume(volume: f32) -> Result<()> {
    save_setting_value("volume", &format!("{:.2}", volume.clamp(0.0, MAX_VOLUME)))
}

pub(crate) fn load_setting_value(key: &str) -> Option<String> {
    fs::read_to_string(SETTINGS_FILE).ok().and_then(|text| {
        text.lines().find_map(|line| {
            let (found_key, value) = line.split_once('=')?;
            (found_key.trim() == key).then(|| value.trim().to_string())
        })
    })
}

pub(crate) fn save_setting_value(key: &str, value: &str) -> Result<()> {
    fs::create_dir_all(DATA_DIR)?;
    let existing = fs::read_to_string(SETTINGS_FILE).unwrap_or_default();
    let mut lines = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line
            .split_once('=')
            .is_some_and(|(found_key, _)| found_key.trim() == key)
        {
            if !replaced {
                lines.push(format!("{key}={value}"));
                replaced = true;
            }
        } else if !line.trim().is_empty() {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(format!("{key}={value}"));
    }
    fs::write(SETTINGS_FILE, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

pub(crate) fn load_setting_bool(key: &str, default: bool) -> bool {
    load_setting_value(key)
        .and_then(|value| match value.as_str() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

pub(crate) fn save_setting_bool(key: &str, value: bool) -> Result<()> {
    save_setting_value(key, if value { "true" } else { "false" })
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

pub(crate) fn prompt_or_escape(message: &str) -> Result<Option<String>> {
    print!("{message}");
    io::stdout().flush()?;
    let _raw = RawMode::new()?;
    let mut value = String::new();
    let mut stdout = io::stdout();
    loop {
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => {
                    write!(stdout, "\r\n")?;
                    stdout.flush()?;
                    return Ok(Some(value));
                }
                KeyCode::Esc => {
                    write!(stdout, "\r\n")?;
                    stdout.flush()?;
                    return Ok(None);
                }
                KeyCode::Backspace => {
                    if value.pop().is_some() {
                        queue!(
                            stdout,
                            cursor::MoveLeft(1),
                            terminal::Clear(ClearType::UntilNewLine)
                        )?;
                        stdout.flush()?;
                    }
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(character);
                    write!(stdout, "{character}")?;
                    stdout.flush()?;
                }
                _ => {}
            },
            Event::Paste(text) => {
                let text = text.replace(['\r', '\n'], " ");
                value.push_str(&text);
                write!(stdout, "{text}")?;
                stdout.flush()?;
            }
            _ => {}
        }
    }
}

pub(crate) fn menu_quit_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Char('q')
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
}

pub(crate) fn bold_text(text: &str) -> String {
    format!("{BOLD_START}{text}{BOLD_END}")
}

fn lyric_color_setting(target: LyricColorTarget) -> LyricColor {
    load_setting_value(target.setting_key())
        .as_deref()
        .map(LyricColor::from_key)
        .unwrap_or(LyricColor::Default)
}

fn transcript_color_setting(target: TranscriptColorTarget) -> LyricColor {
    let default = match target {
        TranscriptColorTarget::Active => LyricColor::Green,
        TranscriptColorTarget::Inactive => LyricColor::Default,
    };
    load_setting_value(target.setting_key())
        .as_deref()
        .map(LyricColor::from_key)
        .unwrap_or(default)
}

fn color_text(text: &str, color: LyricColor) -> String {
    format!("{}{text}{COLOR_DEFAULT}", color.marker())
}

fn marker_color(marker: char) -> Option<Color> {
    match marker {
        COLOR_WHITE => Some(Color::White),
        COLOR_RED => Some(Color::Red),
        COLOR_GREEN => Some(Color::Green),
        COLOR_YELLOW => Some(Color::Yellow),
        COLOR_BLUE => Some(Color::Blue),
        COLOR_MAGENTA => Some(Color::Magenta),
        COLOR_CYAN => Some(Color::Cyan),
        COLOR_DEFAULT => Some(NORMAL_TEXT_COLOR),
        _ => None,
    }
}

fn is_style_marker(character: char) -> bool {
    matches!(character, BOLD_START | BOLD_END) || marker_color(character).is_some()
}

fn is_key_binding(token: &str) -> bool {
    matches!(
        token,
        "Ctrl+Space"
            | "Ctrl+A"
            | "Up/Down"
            | "Left/Right"
            | "Enter"
            | "Esc"
            | "Space"
            | "Backspace"
            | "Delete"
            | "p/Space"
            | "q/Ctrl+C"
            | "n/v"
            | "+/-"
            | "←/→"
            | ",/."
            | "/"
            | "↑/↓"
            | "a"
            | "b"
            | "l"
            | "n"
            | "p"
            | "q"
            | "r"
            | "s"
            | "u"
            | "v"
            | "y"
    )
}

pub(crate) fn bold_key_bindings(text: &str) -> String {
    const KEYS: &[&str] = &[
        "↑/↓",
        "Up/Down",
        "Left/Right",
        "Ctrl+Space",
        "Ctrl+A",
        "q/Ctrl+C",
        "p/Space",
        "Backspace",
        "Delete",
        "Enter",
        "Space",
        "Esc",
        "+/-",
        "n/v",
        "b",
        "u",
        "n",
        "v",
        "r",
        "l",
        "y",
    ];

    let mut output = String::with_capacity(text.len() + 16);
    let mut index = 0;
    while index < text.len() {
        let remaining = &text[index..];
        if let Some(close) = remaining
            .strip_prefix('[')
            .and_then(|value| value.find(']'))
        {
            let end = close + 2;
            if is_key_binding(&remaining[1..end - 1]) {
                output.push_str(&bold_text(&remaining[..end]));
                index += end;
                continue;
            }
        }

        let boundary = index == 0
            || text[..index]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_whitespace() || "|·(".contains(character));
        if boundary
            && let Some(key) = KEYS.iter().find(|key| {
                remaining.starts_with(**key)
                    && remaining[key.len()..]
                        .chars()
                        .next()
                        .is_none_or(|character| {
                            character.is_whitespace() || "|·),".contains(character)
                        })
            })
        {
            output.push_str(&bold_text(key));
            index += key.len();
            continue;
        }

        let character = remaining.chars().next().expect("non-empty remainder");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

fn bold_menu_title(title: &str) -> String {
    if title.starts_with(BOLD_START) {
        return title.to_string();
    }
    if title.starts_with('╭') {
        return bold_text(title);
    }
    title.split_once('\n').map_or_else(
        || bold_text(title),
        |(first, rest)| format!("{}\n{rest}", bold_text(first)),
    )
}

pub(crate) fn select_menu(title: &str, items: &[String]) -> Result<Option<usize>> {
    select_menu_from(title, items, 0)
}

pub(crate) fn select_menu_from(
    title: &str,
    items: &[String],
    initial_selection: usize,
) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let _raw = RawMode::new()?;
    let mut selected = initial_selection.min(items.len() - 1);
    let mut stdout = io::stdout();
    loop {
        let height = terminal::size()
            .map(|(_, height)| height as usize)
            .unwrap_or(24);
        let title_height = if title.is_empty() {
            0
        } else {
            title.lines().count() + 2
        };
        let page_size = height.saturating_sub(title_height + 2).max(1);
        let start = selected
            .saturating_sub(page_size / 2)
            .min(items.len().saturating_sub(page_size));
        let mut frame = if title.is_empty() {
            String::new()
        } else {
            format!("{}\r\n\r\n", bold_menu_title(title))
        };
        for (index, label) in items.iter().enumerate().skip(start).take(page_size) {
            frame.push_str(&format!(
                "{} {label}\r\n",
                if index == selected { ">" } else { " " },
            ));
        }
        frame.push_str(tr("msg.up_down_select_enter_confirm_esc_back_q"));
        draw_frame(&mut stdout, &frame)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if menu_quit_key(key.code, key.modifiers) {
            drop(_raw);
            clear_screen()?;
            std::process::exit(0);
        }
        match key.code {
            KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(items.len() - 1),
            KeyCode::Down => selected = (selected + 1) % items.len(),
            KeyCode::Enter => {
                drop(_raw);
                clear_screen()?;
                return Ok(Some(selected));
            }
            KeyCode::Esc => {
                drop(_raw);
                clear_screen()?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

pub(crate) fn manage_lyrics_settings() -> Result<()> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut editor = LyricsSettingsEditor::new();
    let mut lyrics_visible = load_setting_bool("playback.lyrics", true);
    loop {
        draw_panel(&mut stdout, editor.title(), &editor.rows(lyrics_visible))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if menu_quit_key(key.code, key.modifiers) {
            drop(_raw);
            clear_screen()?;
            std::process::exit(0);
        }
        match editor.handle_key(key.code, key.modifiers, lyrics_visible)? {
            LyricsEditorResult::Visibility(visible) => lyrics_visible = visible,
            LyricsEditorResult::Close => {
                clear_screen()?;
                return Ok(());
            }
            LyricsEditorResult::Handled | LyricsEditorResult::Pass => {}
        }
    }
}

pub(crate) fn manage_transcript_settings() -> Result<()> {
    let _raw = RawMode::new()?;
    let mut stdout = io::stdout();
    let mut editor = TranscriptSettingsEditor::new();
    let mut transcript_visible = load_setting_bool("playback.transcript", true);
    loop {
        draw_panel(
            &mut stdout,
            editor.title(),
            &editor.rows(transcript_visible),
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if menu_quit_key(key.code, key.modifiers) {
            drop(_raw);
            clear_screen()?;
            std::process::exit(0);
        }
        match editor.handle_key(key.code, key.modifiers, transcript_visible)? {
            TranscriptEditorResult::Visibility(visible) => transcript_visible = visible,
            TranscriptEditorResult::Close => {
                clear_screen()?;
                return Ok(());
            }
            TranscriptEditorResult::Handled
            | TranscriptEditorResult::LanguageChanged
            | TranscriptEditorResult::Pass => {}
        }
    }
}

pub(crate) fn toggle_all<T: Eq + Hash>(
    selected: &mut HashSet<T>,
    matches: impl IntoIterator<Item = T>,
) {
    let matches: Vec<T> = matches.into_iter().collect();
    if matches.iter().all(|item| selected.contains(item)) {
        for item in matches {
            selected.remove(&item);
        }
    } else {
        selected.extend(matches);
    }
}

pub(crate) fn manage_queue<T, Label, Matches, Finished>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    available: &[T],
    label: Label,
    matches_query: Matches,
    playback_finished: Finished,
) -> Result<QueueEdit>
where
    T: Clone + Eq + Hash,
    Label: Fn(&T) -> String,
    Matches: Fn(&T, &str) -> bool,
    Finished: FnMut() -> bool,
{
    manage_queue_inner(
        queue,
        current,
        play_next,
        &label,
        |queue, current, play_next, playback_finished| {
            append_to_queue(
                queue,
                current,
                play_next,
                available,
                &label,
                &matches_query,
                playback_finished,
            )
        },
        playback_finished,
    )
}

pub(crate) fn manage_queue_with_adder<T, Label, Add, Finished>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    label: Label,
    mut add: Add,
    playback_finished: Finished,
) -> Result<QueueEdit>
where
    T: Clone + Eq + Hash,
    Label: Fn(&T) -> String,
    Add: FnMut() -> Result<Vec<T>>,
    Finished: FnMut() -> bool,
{
    manage_queue_inner(
        queue,
        current,
        play_next,
        &label,
        |queue, current, play_next, _| {
            let additions = add()?;
            let changed = !additions.is_empty();
            insert_queue_next(queue, current, play_next, additions);
            Ok(changed)
        },
        playback_finished,
    )
}

fn manage_queue_inner<T, Label, Add, Finished>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    label: &Label,
    mut add: Add,
    mut playback_finished: Finished,
) -> Result<QueueEdit>
where
    T: Clone + Eq + Hash,
    Label: Fn(&T) -> String,
    Add: FnMut(&mut Vec<T>, usize, &mut usize, &mut Finished) -> Result<bool>,
    Finished: FnMut() -> bool,
{
    let current = current.min(queue.len().saturating_sub(1));
    let current_item = queue[current].clone();
    let mut selected = current;
    let mut stdout = io::stdout();
    loop {
        draw_frame(
            &mut stdout,
            &queue_table(queue, current, *play_next, selected, label),
        )?;

        if !event::poll(Duration::from_millis(200))? {
            if playback_finished() {
                return Ok(QueueEdit {
                    index: current,
                    restart: false,
                    finished: true,
                });
            }
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected = if selected <= current {
                    queue.len() - 1
                } else {
                    selected - 1
                }
            }
            KeyCode::Down => {
                selected = if selected + 1 >= queue.len() {
                    current
                } else {
                    selected + 1
                }
            }
            KeyCode::Enter => {
                if selected != current {
                    *play_next = 0;
                }
                return Ok(QueueEdit {
                    index: selected,
                    restart: queue[selected] != current_item,
                    finished: false,
                });
            }
            KeyCode::Delete
                if selected > current && selected <= current.saturating_add(*play_next) =>
            {
                unqueue_next_item(queue, current, play_next, selected);
                selected = selected.min(queue.len() - 1);
            }
            KeyCode::Char('a') => {
                add(queue, current, play_next, &mut playback_finished)?;
                selected = selected.min(queue.len() - 1);
            }
            KeyCode::Esc => {
                return Ok(QueueEdit {
                    index: current,
                    restart: queue[current] != current_item,
                    finished: false,
                });
            }
            _ => {}
        }
    }
}

fn queue_table<T, Label>(
    queue: &[T],
    current: usize,
    play_next: usize,
    selected: usize,
    label: &Label,
) -> String
where
    Label: Fn(&T) -> String,
{
    let (terminal_width, terminal_height) = terminal::size().unwrap_or((80, 24));
    queue_table_at_size(
        queue,
        current,
        play_next,
        selected,
        label,
        terminal_width,
        terminal_height,
    )
}

fn queue_table_at_size<T, Label>(
    queue: &[T],
    current: usize,
    play_next: usize,
    selected: usize,
    label: &Label,
    terminal_width: u16,
    terminal_height: u16,
) -> String
where
    Label: Fn(&T) -> String,
{
    let page_size = (terminal_height as usize).saturating_sub(9).clamp(1, 12);
    if terminal_width < 75 {
        let width = (terminal_width as usize).saturating_sub(4).max(1);
        let mut frame = format!(
            "{} | {} {}\r\n\r\n",
            tr("msg.playback_queue"),
            queue.len(),
            tr("msg.songs")
        );
        frame.push_str(&format!(
            "{}\r\n{} {}\r\n",
            bold_text(tr("msg.now_playing")),
            if selected == current { '>' } else { ' ' },
            fit_text(&label(&queue[current]), width)
        ));
        let start = current.saturating_add(1);
        let end = queue.len();
        let window = queue_window_start(selected, start, end, page_size);
        for index in start.saturating_add(window)..end.min(start.saturating_add(window + page_size))
        {
            let section = if index <= current.saturating_add(play_next) {
                tr("msg.next_in_queue")
            } else {
                tr("msg.next_from_track_list")
            };
            frame.push_str(&format!(
                "{} {}: {}\r\n",
                if index == selected { ">" } else { " " },
                section,
                fit_text(
                    &label(&queue[index]),
                    width.saturating_sub(visible_text_width(section) + 4).max(1)
                )
            ));
        }
        frame.push_str(tr("msg.up_down_select_enter_play_delete_unqueue_a"));
        return frame;
    }
    let width = ((terminal_width as usize).saturating_sub(4) / 3).min(34);
    let queued_start = current.saturating_add(1).min(queue.len());
    let queued_end = queued_start.saturating_add(play_next).min(queue.len());
    let remaining_start = queued_end;
    let queued_offset = queue_window_start(selected, queued_start, queued_end, page_size);
    let remaining_offset = queue_window_start(selected, remaining_start, queue.len(), page_size);
    let queued_visible_start = queued_start + queued_offset;
    let remaining_visible_start = remaining_start + remaining_offset;
    let queued = &queue[queued_visible_start..queued_end.min(queued_visible_start + page_size)];
    let remaining =
        &queue[remaining_visible_start..queue.len().min(remaining_visible_start + page_size)];
    let rows = queued.len().max(remaining.len()).max(1);
    let cell = |index: Option<usize>, item: Option<&T>| {
        item.map(|item| {
            fit_text(
                &format!(
                    "{} {}",
                    if index == Some(selected) { ">" } else { " " },
                    label(item)
                ),
                width,
            )
        })
        .unwrap_or_else(|| " ".repeat(width))
    };
    let mut frame = format!(
        "{} | {} {}\r\n\r\n┌{}┬{}┬{}┐\r\n│{}│{}│{}│\r\n├{}┼{}┼{}┤\r\n",
        tr("msg.playback_queue"),
        queue.len(),
        tr("msg.songs"),
        "─".repeat(width),
        "─".repeat(width),
        "─".repeat(width),
        bold_text(&fit_text(tr("msg.now_playing"), width)),
        bold_text(&fit_text(tr("msg.next_in_queue"), width)),
        bold_text(&fit_text(tr("msg.next_from_track_list"), width)),
        "─".repeat(width),
        "─".repeat(width),
        "─".repeat(width),
    );
    for row in 0..rows {
        frame.push_str(&format!(
            "│{}│{}│{}│\r\n",
            cell(
                (row == 0).then_some(current),
                (row == 0).then_some(&queue[current])
            ),
            cell(
                (row < queued.len()).then_some(queued_visible_start + row),
                queued.get(row),
            ),
            cell(
                (row < remaining.len()).then_some(remaining_visible_start + row),
                remaining.get(row),
            ),
        ));
    }
    frame.push_str(&format!(
        "└{}┴{}┴{}┘\r\n\r\n{}",
        "─".repeat(width),
        "─".repeat(width),
        "─".repeat(width),
        tr("msg.up_down_select_enter_play_delete_unqueue_a"),
    ));
    frame
}

pub(crate) fn queue_window_start(
    selected: usize,
    segment_start: usize,
    segment_end: usize,
    page_size: usize,
) -> usize {
    let length = segment_end.saturating_sub(segment_start);
    if selected < segment_start || selected >= segment_end {
        return 0;
    }
    (selected - segment_start)
        .saturating_sub(page_size / 2)
        .min(length.saturating_sub(page_size))
}

pub(crate) fn previous_track_index(
    current: usize,
    track_count: usize,
    resume_after_replay: &mut Option<usize>,
) -> usize {
    resume_after_replay.get_or_insert(current);
    if current == 0 {
        track_count - 1
    } else {
        current - 1
    }
}

pub(crate) fn forward_track_index(
    current: usize,
    track_count: usize,
    loop_mode: LoopMode,
    natural_end: bool,
    resume_after_replay: &mut Option<usize>,
) -> Option<(usize, bool, bool)> {
    if let Some(resume) = resume_after_replay.take() {
        return Some((resume, false, false));
    }
    next_track_index(current, track_count, loop_mode, natural_end)
        .map(|next| (next, next == 0 && next != current, true))
}

pub(crate) fn unqueue_next_item<T: Eq>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    remove: usize,
) {
    let item = queue.remove(remove);
    *play_next -= 1;
    let tail_start = current
        .saturating_add(1)
        .saturating_add(*play_next)
        .min(queue.len());
    if !queue[tail_start..].contains(&item) {
        queue.push(item);
    }
}

pub(crate) fn insert_queue_next<T>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    additions: Vec<T>,
) {
    let insert_at = current
        .saturating_add(1)
        .saturating_add(*play_next)
        .min(queue.len());
    *play_next += additions.len();
    queue.splice(insert_at..insert_at, additions);
}

pub(crate) fn set_shuffle_order<T: Clone + Eq>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: usize,
    original: &[T],
    shuffle: bool,
) {
    let tail_start = current
        .saturating_add(1)
        .saturating_add(play_next)
        .min(queue.len());
    if shuffle {
        shuffle_slice(&mut queue[tail_start..]);
        return;
    }

    let mut tail: Vec<_> = queue.drain(tail_start..).collect();
    restore_order(&mut tail, original);
    queue.extend(tail);
}

pub(crate) fn restart_pass_order<T: Clone + Eq>(queue: &mut Vec<T>, original: &[T], shuffle: bool) {
    if shuffle {
        shuffle_slice(queue);
    } else {
        restore_order(queue, original);
    }
}

fn restore_order<T: Clone + Eq>(items: &mut Vec<T>, original: &[T]) {
    let mut remaining = std::mem::take(items);
    let mut ordered = Vec::with_capacity(remaining.len());
    for original_item in original {
        if let Some(index) = remaining.iter().position(|item| item == original_item) {
            ordered.push(remaining.remove(index));
        }
    }
    ordered.append(&mut remaining);
    *items = ordered;
}

pub(crate) fn next_track_index(
    current: usize,
    track_count: usize,
    loop_mode: LoopMode,
    natural_end: bool,
) -> Option<usize> {
    if natural_end && loop_mode == LoopMode::One {
        return Some(current);
    }
    if current + 1 < track_count {
        return Some(current + 1);
    }
    (loop_mode == LoopMode::All).then_some(0)
}

fn append_to_queue<T, Label, Matches, Finished>(
    queue: &mut Vec<T>,
    current: usize,
    play_next: &mut usize,
    available: &[T],
    label: &Label,
    matches_query: &Matches,
    playback_finished: &mut Finished,
) -> Result<bool>
where
    T: Clone + Eq + Hash,
    Label: Fn(&T) -> String,
    Matches: Fn(&T, &str) -> bool,
    Finished: FnMut() -> bool,
{
    let mut query = String::new();
    let mut matches: Vec<_> = (0..available.len()).collect();
    let mut row = 0usize;
    let mut selected = HashSet::new();
    let mut stdout = io::stdout();
    loop {
        row = row.min(matches.len().saturating_sub(1));
        let start = row.saturating_sub(6).min(matches.len().saturating_sub(12));
        let end = (start + 12).min(matches.len());
        let mut frame = format!(
            "Add songs to queue\r\nSearch: {query}_ | {} selected | {} matches\r\n\r\n",
            selected.len(),
            matches.len()
        );
        for (offset, &index) in matches[start..end].iter().enumerate() {
            frame.push_str(&format!(
                "{} [{}] {}\r\n",
                if start + offset == row { ">" } else { " " },
                if selected.contains(&available[index]) {
                    "x"
                } else {
                    " "
                },
                label(&available[index])
            ));
        }
        if matches.is_empty() {
            frame.push_str("No matching tracks.\r\n");
        }
        frame.push_str(
            "\r\nType to search | [Ctrl+Space] toggle | [Ctrl+A] all matches | [Enter] append | [Esc] cancel",
        );
        draw_frame(&mut stdout, &frame)?;

        if playback_finished() {
            return Ok(false);
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
            KeyCode::Up if !matches.is_empty() => {
                row = row.checked_sub(1).unwrap_or(matches.len() - 1)
            }
            KeyCode::Down if !matches.is_empty() => row = (row + 1) % matches.len(),
            KeyCode::Char(' ') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                if !matches.is_empty() {
                    let item = &available[matches[row]];
                    if !selected.remove(item) {
                        selected.insert(item.clone());
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                toggle_all(
                    &mut selected,
                    matches.iter().map(|&index| available[index].clone()),
                )
            }
            KeyCode::Backspace => {
                query.pop();
                matches = available
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| matches_query(item, &query).then_some(index))
                    .collect();
                row = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(event::KeyModifiers::CONTROL)
                    && !character.is_control() =>
            {
                query.push(character);
                matches = available
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| matches_query(item, &query).then_some(index))
                    .collect();
                row = 0;
            }
            KeyCode::Enter => {
                let additions: Vec<_> = available
                    .iter()
                    .filter(|item| selected.contains(*item))
                    .cloned()
                    .collect();
                let changed = !additions.is_empty();
                insert_queue_next(queue, current, play_next, additions);
                return Ok(changed);
            }
            KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

#[cfg(test)]
pub(crate) fn playback_controls_with_lyrics() -> Vec<String> {
    playback_controls_with_lyrics_at_width(player_panel_width())
}

pub(crate) fn playback_controls_with_lyrics_at_width(width: usize) -> Vec<String> {
    playback_controls_with_middle(tr("msg.b_lyrics_settings"), false, width)
}

#[cfg(test)]
pub(crate) fn playback_controls_with_transcript() -> Vec<String> {
    playback_controls_with_middle(tr("msg.b_transcript_settings"), false, player_panel_width())
}

pub(crate) fn playback_controls_with_youtube() -> Vec<String> {
    playback_controls_with_middle(tr("msg.b_transcript_settings"), true, player_panel_width())
}

pub(crate) fn playback_key_bindings_hint() -> String {
    bold_key_bindings(tr("msg.hide_playback_key_bindings"))
}

pub(crate) fn toggle_playback_key_bindings() -> Result<()> {
    let visible = !load_setting_bool("playback.key_bindings", true);
    save_setting_bool("playback.key_bindings", visible)
}

fn playback_controls_with_middle(middle: &str, youtube: bool, width: usize) -> Vec<String> {
    let mut controls = vec![
        tr("msg.p_play_pause"),
        tr("msg.v_previous"),
        tr("msg.n_next"),
        tr("msg.r_shuffle"),
        tr("msg.l_loop"),
        tr("msg.u_queue"),
        tr("msg.volume"),
        middle,
        tr("msg.q_quit"),
    ];
    if youtube {
        controls.extend(
            tr("msg.seek_10s_0_25x_s_sponsors")
                .split('│')
                .map(str::trim)
                .filter(|cell| !cell.is_empty()),
        );
    }
    playback_control_grid(&controls, width)
}

fn playback_control_grid(controls: &[&str], width: usize) -> Vec<String> {
    let columns = if width >= 52 {
        3
    } else if width >= 35 {
        2
    } else {
        1
    };
    if width < 18 {
        return controls
            .iter()
            .map(|control| fit_text(control, width))
            .collect();
    }
    let cell_width = (width.saturating_sub(columns * 3 + 1) / columns).min(14);
    let border = |left: char, join: char, right: char| {
        let segments = vec!["─".repeat(cell_width + 2); columns];
        format!("{left}{}{right}", segments.join(&join.to_string()))
    };
    let mut lines = vec![border('┌', '┬', '┐')];
    for (index, group) in controls.chunks(columns).enumerate() {
        if index > 0 && columns > 1 {
            lines.push(border('├', '┼', '┤'));
        }
        let cells = (0..columns)
            .map(|col| {
                format!(
                    " {} ",
                    fit_text(group.get(col).copied().unwrap_or(""), cell_width)
                )
            })
            .collect::<Vec<_>>();
        lines.push(format!("│{}│", cells.join("│")));
    }
    lines.push(border('└', '┴', '┘'));
    lines
}

pub(crate) fn player_panel_width() -> usize {
    panel_available_width(terminal::size().map(|size| size.0).unwrap_or(80)).min(52)
}

fn panel_available_width(columns: u16) -> usize {
    usize::from(columns).saturating_sub(5)
}

pub(crate) fn playback_bar_width(time_text: &str) -> usize {
    player_panel_width()
        .saturating_sub(visible_text_width(time_text) + 3)
        .clamp(1, 12)
}

pub(crate) fn playback_bar_width_at_width(time_text: &str, width: usize) -> usize {
    width
        .saturating_sub(visible_text_width(time_text) + 3)
        .min(12)
}

pub(crate) fn playback_progress_row_at_width(
    elapsed: Duration,
    total: Option<Duration>,
    time_text: &str,
    width: usize,
) -> String {
    if visible_text_width(time_text) + 3 > width {
        return fit_text(time_text, width);
    }
    format!(
        "[{}] {time_text}",
        progress_bar(
            elapsed,
            total,
            playback_bar_width_at_width(time_text, width)
        )
    )
}

pub(crate) fn wrap_playback_status(text: &str) -> Vec<String> {
    wrap_playback_status_at_width(text, player_panel_width())
}

pub(crate) fn playback_metadata_fits(progress: &str, metadata: &str) -> bool {
    visible_text_width(progress) + 3 + visible_text_width(metadata) <= player_panel_width()
}

pub(crate) fn wrap_playback_status_at_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for field in text.split(" · ") {
        let candidate = if current.is_empty() {
            field.to_string()
        } else {
            format!("{current} · {field}")
        };
        if visible_text_width(&candidate) <= width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        for word in field.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if visible_text_width(&candidate) > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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

pub(crate) fn playback_progress_or_volume(
    progress: String,
    volume: f32,
    last_change: Option<Instant>,
) -> String {
    playback_progress_or_volume_at_width(progress, volume, last_change, player_panel_width())
}

pub(crate) fn playback_progress_or_volume_at_width(
    progress: String,
    volume: f32,
    last_change: Option<Instant>,
    available_width: usize,
) -> String {
    if !last_change.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
        return progress;
    }
    let percentage = format!("{:.0}%", volume.clamp(0.0, 1.0) * 100.0);
    let label = tr("msg.volume_meter");
    let width = available_width
        .saturating_sub(visible_text_width(label) + visible_text_width(&percentage) + 4)
        .min(12);
    let filled = (volume.clamp(0.0, 1.0) * width as f32).round() as usize;
    format!(
        "{label} [{}{}] {percentage}",
        "/".repeat(filled),
        " ".repeat(width - filled)
    )
}

#[cfg(test)]
mod responsive_player_tests {
    use super::*;

    #[test]
    fn controls_use_available_columns_and_preserve_panel_borders() {
        let controls = [
            "[p] play/pause",
            "[v] previous",
            "[n] next",
            "[r] shuffle",
            "[l] loop",
            "[u] queue",
            "[↑/↓] volume",
            "[b] lyrics",
            "[q] quit",
        ];
        for terminal_columns in [20_u16, 24, 40, 52, 80, 120] {
            let content_width = panel_available_width(terminal_columns).min(52);
            let rendered = playback_control_grid(&controls, content_width);
            assert!(
                rendered
                    .iter()
                    .all(|line| visible_text_width(line) <= content_width)
            );
            let expected_columns = if content_width >= 52 {
                3
            } else if content_width >= 35 {
                2
            } else {
                1
            };
            if content_width >= 18 {
                assert!(rendered.first().unwrap().starts_with('┌'));
                assert!(rendered.last().unwrap().ends_with('┘'));
                assert_eq!(rendered[0].matches('┬').count(), expected_columns - 1);
                assert!(
                    !rendered
                        .iter()
                        .any(|line| line.starts_with('┌') && line.contains('…'))
                );
                if expected_columns == 1 {
                    assert_eq!(rendered.len(), controls.len() + 2);
                }
            } else {
                assert_eq!(rendered.len(), controls.len());
            }
            for line in panel_lines("Music Terminal", &rendered, content_width, false) {
                assert!(visible_text_width(&line) <= usize::from(terminal_columns) - 1);
            }
        }
    }

    #[test]
    fn narrow_status_wraps_at_separators_and_respects_unicode_markers() {
        let status = "playing · cache 100% · shuffle off · loop off";
        assert_eq!(
            wrap_playback_status_at_width(status, 24),
            ["playing · cache 100%", "shuffle off · loop off"]
        );
        assert_eq!(
            wrap_playback_status_at_width(status, 20),
            ["playing · cache 100%", "shuffle off", "loop off"]
        );
        let styled = format!("{} · cache 100%", bold_text("đang phát"));
        let wrapped = wrap_playback_status_at_width(&styled, 12);
        assert!(wrapped.iter().all(|line| visible_text_width(line) <= 12));
        assert!(wrapped.concat().contains("đang"));
    }

    #[test]
    fn short_heights_preserve_controls_or_request_resize() {
        let mut rows = vec![
            "Track 1/9".into(),
            "".into(),
            "[////] 1:00/3:00".into(),
            "playing".into(),
            "".into(),
        ];
        rows.extend(playback_control_grid(
            &["[p] play", "[n] next", "[q] quit"],
            19,
        ));
        let compact = player_rows_for_height(&rows, 24 - 1 - 4).unwrap();
        assert!(compact.iter().any(|line| line.ends_with('┘')));
        assert!(player_rows_for_height(&rows, 10 - 1 - 4).is_none());
    }

    #[test]
    fn narrow_volume_meter_preserves_percentage() {
        let changed = Some(Instant::now());
        let compact = playback_progress_or_volume_at_width("progress".into(), 0.67, changed, 18);
        assert!(compact.starts_with("Volume ["));
        assert!(compact.ends_with("67%"));
        assert!(visible_text_width(&compact) <= 18);
    }

    #[test]
    fn adaptive_lyrics_player_rows_fit_the_allocated_width() {
        let elapsed = Duration::from_secs(8);
        let duration = Duration::from_secs(150);
        let time_text = "0:08/2:30";
        for width in [16, 17, 29, 41, 52] {
            let title =
                player_title_at_width("Track", 0, 907, "Tiếng Việt - a long song title", width);
            assert!(visible_text_width(&title) <= width, "title width {width}");

            let progress =
                playback_progress_row_at_width(elapsed, Some(duration), time_text, width);
            assert!(
                visible_text_width(&progress) <= width,
                "progress width {width}"
            );
            let long_time =
                playback_progress_row_at_width(elapsed, Some(duration), "1:02:34/2:03:45", width);
            assert!(
                visible_text_width(&long_time) <= width,
                "long time width {width}"
            );
            if width == 16 {
                assert_eq!(long_time.trim_end(), "1:02:34/2:03:45");
            }
            let meter = playback_progress_or_volume_at_width(
                progress.clone(),
                0.67,
                Some(Instant::now()),
                width,
            );
            assert!(visible_text_width(&meter) <= width, "volume width {width}");
            assert_eq!(
                playback_progress_or_volume_at_width(progress.clone(), 0.67, None, width),
                progress
            );

            let controls = playback_controls_with_lyrics_at_width(width);
            assert!(
                controls
                    .iter()
                    .all(|line| visible_text_width(line) <= width)
            );
            assert!(controls.iter().any(|line| line.contains("[p]")));
            assert!(controls.iter().any(|line| line.contains("[q]")));

            let status = wrap_playback_status_at_width(
                "playing · cache 100% · shuffle off · loop off",
                width,
            );
            assert!(status.iter().all(|line| visible_text_width(line) <= width));
            assert!(status.join(" ").contains("loop off"));
        }
    }

    #[test]
    fn volume_replaces_progress_only_for_one_second_from_latest_press() {
        let progress = "[////        ] 0:04/3:00".to_string();
        let earlier = Instant::now() - Duration::from_millis(1100);
        assert_eq!(
            playback_progress_or_volume(progress.clone(), 0.5, None),
            progress
        );
        assert_eq!(
            playback_progress_or_volume(progress.clone(), 0.5, Some(earlier)),
            progress
        );
        let first = Instant::now() - Duration::from_millis(900);
        let second = Instant::now();
        assert_eq!(
            playback_progress_or_volume(progress.clone(), 0.5, Some(first)),
            "Volume [//////      ] 50%"
        );
        assert_eq!(
            playback_progress_or_volume(progress, 0.8, Some(second)),
            "Volume [//////////  ] 80%"
        );
    }

    #[test]
    fn volume_bar_matches_playback_style_at_zero_partial_and_full_volume() {
        let progress = "[////        ] 0:04/3:00".to_string();
        let changed = Some(Instant::now());
        for (volume, expected) in [
            (0.0, "Volume [            ] 0%"),
            (0.25, "Volume [///         ] 25%"),
            (0.67, "Volume [////////    ] 67%"),
            (1.0, "Volume [////////////] 100%"),
        ] {
            assert_eq!(
                playback_progress_or_volume(progress.clone(), volume, changed),
                expected
            );
        }
    }

    #[test]
    fn queue_stacks_in_narrow_terminal_and_keeps_selection_visible() {
        let queue = ["first", "second", "third"];
        let label = |entry: &&str| entry.to_string();
        let narrow = queue_table_at_size(&queue, 0, 1, 2, &label, 52, 20);
        let wide = queue_table_at_size(&queue, 0, 1, 2, &label, 100, 20);
        assert!(narrow.contains(">"));
        assert!(narrow.contains("third"));
        assert!(!narrow.contains("┬"));
        assert!(wide.contains("┬"));
        assert!(wide.contains("third"));
    }
}

pub(crate) fn fit_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let visible_width = text
        .chars()
        .filter(|character| !is_style_marker(*character))
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>();
    if visible_width <= width {
        return format!("{text}{}", " ".repeat(width - visible_width));
    }

    let content_width = width.saturating_sub(1);
    let mut value = String::new();
    let mut used = 0usize;
    let mut bold = false;
    let mut colored = false;
    for character in text.chars() {
        if character == BOLD_START {
            bold = true;
            value.push(character);
            continue;
        }
        if character == BOLD_END {
            bold = false;
            value.push(character);
            continue;
        }
        if marker_color(character).is_some() {
            colored = character != COLOR_DEFAULT;
            value.push(character);
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        value.push(character);
        used += character_width;
    }
    if bold {
        value.push(BOLD_END);
    }
    if colored {
        value.push(COLOR_DEFAULT);
    }
    value.push('…');
    value.push_str(&" ".repeat(width.saturating_sub(used + 1)));
    value
}

pub(crate) fn marquee_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }

    let chars = text.chars().collect::<Vec<_>>();
    let widths = chars
        .iter()
        .map(|character| UnicodeWidthChar::width(*character).unwrap_or(0))
        .collect::<Vec<_>>();
    let total_width = widths.iter().sum::<usize>();

    let mut suffix_width = 0usize;
    let mut last_start = chars.len().saturating_sub(1);
    for index in (0..chars.len()).rev() {
        suffix_width += widths[index];
        if suffix_width > width {
            last_start = index + 1;
            break;
        }
        last_start = index;
    }

    let positions = last_start + 1;
    let step_ms = 200u128;
    let pause_steps = 5usize;
    let travel_steps = positions.saturating_sub(1);
    let cycle_steps = pause_steps + travel_steps + pause_steps + travel_steps;
    let now_steps = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / step_ms;
    let phase = if cycle_steps == 0 {
        0
    } else {
        (now_steps as usize) % cycle_steps
    };
    let start = if phase < pause_steps {
        0
    } else if phase < pause_steps + travel_steps {
        phase - pause_steps + 1
    } else if phase < pause_steps + travel_steps + pause_steps {
        last_start
    } else {
        last_start.saturating_sub(phase - (pause_steps + travel_steps + pause_steps) + 1)
    };

    let mut rendered = String::new();
    let mut used = 0usize;
    for (character, character_width) in chars[start..].iter().zip(&widths[start..]) {
        if used + *character_width > width {
            break;
        }
        rendered.push(*character);
        used += *character_width;
    }
    if start == last_start && used < width && total_width > width {
        rendered.push_str(&" ".repeat(width - used));
    }
    rendered
}

pub(crate) fn player_title_row(label: &str, index: usize, total: usize, title: &str) -> String {
    player_title_at_width(label, index, total, title, player_panel_width())
}

pub(crate) fn player_title_at_width(
    label: &str,
    index: usize,
    total: usize,
    title: &str,
    width: usize,
) -> String {
    let mut prefix = format!("{label} {}/{} | ", index + 1, total);
    if visible_text_width(&prefix) + 5 > width {
        prefix = format!("{}/{} ", index + 1, total);
    }
    if visible_text_width(&prefix) + 2 > width {
        prefix = format!("#{} ", index + 1);
    }
    let title_width = width.saturating_sub(visible_text_width(&prefix));
    format!("{prefix}{}", bold_text(&marquee_text(title, title_width)))
}

pub(crate) fn draw_frame(stdout: &mut io::Stdout, frame: &str) -> Result<()> {
    let mut output = Vec::new();
    queue!(output, cursor::MoveTo(0, 0))?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    for (index, line) in frame
        .lines()
        .take(rows.saturating_sub(1) as usize)
        .enumerate()
    {
        if index > 0 {
            queue!(output, cursor::MoveToNextLine(1))?;
        }
        queue!(
            output,
            terminal::Clear(ClearType::CurrentLine),
            cursor::MoveToColumn(0),
            SetForegroundColor(NORMAL_TEXT_COLOR)
        )?;
        let line = fit_text(line.trim_end_matches('\r'), cols.saturating_sub(1) as usize);
        let line = line.trim_end();
        let line = if index == 0 && !line.contains(BOLD_START) {
            bold_text(line)
        } else {
            line.to_string()
        };
        let styled_line = bold_key_bindings(&line);
        for character in styled_line.chars() {
            match character {
                BOLD_START => queue!(output, SetAttribute(Attribute::Bold))?,
                BOLD_END => queue!(output, SetAttribute(Attribute::NormalIntensity))?,
                marker if marker_color(marker).is_some() => {
                    queue!(output, SetForegroundColor(marker_color(marker).unwrap()))?
                }
                character => {
                    let mut encoded = [0; 4];
                    output.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
                }
            }
        }
    }
    queue!(
        output,
        SetAttribute(Attribute::Reset),
        ResetColor,
        terminal::Clear(ClearType::FromCursorDown)
    )?;
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

pub(crate) fn draw_panel(stdout: &mut io::Stdout, title: &str, rows: &[String]) -> Result<()> {
    let available = panel_available_width(terminal::size().map(|size| size.0).unwrap_or(80));
    let content = rows
        .iter()
        .map(|row| visible_text_width(row))
        .chain([visible_text_width(title)])
        .max()
        .unwrap_or(28);
    let width = content.clamp(1, 60).min(available);
    let mut frame = String::new();
    frame.push_str(&format!("┌{}┐\r\n", "─".repeat(width + 2)));
    frame.push_str(&format!("│ {} │\r\n", bold_text(&fit_text(title, width))));
    frame.push_str(&format!("├{}┤\r\n", "─".repeat(width + 2)));
    for row in rows {
        frame.push_str(&format!("│ {} │\r\n", fit_text(row, width)));
    }
    frame.push_str(&format!("└{}┘", "─".repeat(width + 2)));

    draw_frame(stdout, &frame)
}

fn player_rows_for_height(rows: &[String], limit: usize) -> Option<Vec<String>> {
    let mut visible = rows.to_vec();
    while visible.last().is_some_and(String::is_empty) {
        visible.pop();
    }
    while visible.len() > limit {
        if let Some(index) = visible.iter().position(String::is_empty) {
            visible.remove(index);
        } else if let Some(index) = visible
            .iter()
            .position(|row| row.starts_with('├') && row.ends_with('┤'))
        {
            visible.remove(index);
        } else if let Some(index) = visible
            .iter()
            .position(|row| *row == playback_key_bindings_hint())
        {
            visible.remove(index);
        } else {
            return None;
        }
    }
    Some(visible)
}

fn small_terminal(columns: u16, height: u16) -> bool {
    columns < 18 || height < 10
}

fn resize_terminal_message(stdout: &mut io::Stdout) -> Result<()> {
    let columns = terminal::size().map(|size| size.0).unwrap_or(80);
    let message = if columns < 8 {
        "Resize"
    } else if columns < 18 {
        "Resize window"
    } else {
        "Resize terminal"
    };
    draw_frame(stdout, message)
}

fn lyrics_panel_layout(terminal_width: usize, prefer_side: bool) -> Option<(usize, usize, bool)> {
    let default_width = terminal_width.saturating_sub(5).min(52);
    if !prefer_side {
        return Some((default_width, default_width, false));
    }

    // Two framed panels (four columns each), their gap, and the reserved terminal column.
    let budget = terminal_width.saturating_sub(10);
    if budget < 16 + 14 {
        return None;
    }
    if budget >= 104 {
        return Some((52, 52, true));
    }

    let mut player = (budget * 42 / 100).clamp(16, 52);
    let lyrics = (budget - player).min(52);
    player += (budget - player - lyrics).min(52 - player);
    Some((player, lyrics, true))
}

pub(crate) fn lyrics_player_panel_width(show_lyrics: bool, editing_lyrics: bool) -> usize {
    if !show_lyrics && !editing_lyrics {
        return player_panel_width();
    }
    let columns = terminal::size().map(|size| size.0).unwrap_or(80);
    let prefer_side = load_setting_value("lyrics.position").as_deref() != Some("below");
    lyrics_panel_layout(usize::from(columns), prefer_side)
        .map(|(width, _, _)| width)
        .unwrap_or(16)
}

#[cfg(test)]
mod lyrics_panel_layout_tests {
    use super::{lyrics_panel_layout, panel_lines, visible_text_width};

    #[test]
    fn lyrics_stay_beside_player_within_terminal_width() {
        for columns in [20_u16, 24, 40, 52, 80, 120] {
            let layout = lyrics_panel_layout(usize::from(columns), true);
            if columns < 40 {
                assert!(layout.is_none(), "terminal width {columns}");
                continue;
            }
            let (player_width, lyrics_width, side_by_side) = layout.unwrap();
            let player = panel_lines("Player", &["playing".to_string()], player_width, false);
            let lyrics = panel_lines("Lyrics", &["Lời bài hát".to_string()], lyrics_width, true);

            assert!(side_by_side, "terminal width {columns}");
            assert!(player_width >= 16 && lyrics_width >= 14);
            assert!(player_width <= 52 && lyrics_width <= 52);
            assert!(visible_text_width(&player[0]) < usize::from(columns));
            assert!(
                visible_text_width(&player[0]) + visible_text_width(&lyrics[0]) + 1
                    < usize::from(columns)
            );
            assert_eq!(player[0].matches('┌').count(), 1);
        }
        assert_eq!(lyrics_panel_layout(40, true), Some((16, 14, true)));
        assert_eq!(lyrics_panel_layout(52, true), Some((17, 25, true)));
        assert_eq!(lyrics_panel_layout(80, true), Some((29, 41, true)));
        assert_eq!(lyrics_panel_layout(120, true), Some((52, 52, true)));
        assert_eq!(lyrics_panel_layout(120, false), Some((52, 52, false)));
        assert_eq!(lyrics_panel_layout(40, false), Some((35, 35, false)));
    }
}

pub(crate) fn draw_player_panels(
    stdout: &mut io::Stdout,
    title: &str,
    rows: &[String],
    lyrics: Option<&Lyrics>,
    elapsed: Duration,
    lyrics_scroll: usize,
    show_lyrics: bool,
    lyrics_editor: Option<&LyricsSettingsEditor>,
) -> Result<()> {
    let (terminal_columns, terminal_height) = terminal::size().unwrap_or((80, 24));
    if small_terminal(terminal_columns, terminal_height) {
        return resize_terminal_message(stdout);
    }
    let usable_height = usize::from(terminal_height.saturating_sub(1));
    let terminal_width = usize::from(terminal_columns);
    if !show_lyrics && lyrics_editor.is_none() {
        let Some(visible) = player_rows_for_height(rows, usable_height.saturating_sub(4)) else {
            return resize_terminal_message(stdout);
        };
        return draw_panel(stdout, title, &visible);
    }
    let prefer_side = load_setting_value("lyrics.position").as_deref() != Some("below");
    let center_lyrics = load_setting_value("lyrics.alignment").as_deref() == Some("center");
    let Some((left_width, lyrics_width, side_by_side)) =
        lyrics_panel_layout(terminal_width, prefer_side)
    else {
        return resize_terminal_message(stdout);
    };
    let target_height = usable_height.saturating_sub(if side_by_side { 4 } else { 9 });
    let Some(mut left_rows) = player_rows_for_height(rows, target_height)
        .or_else(|| player_rows_for_height(rows, usable_height.saturating_sub(4)))
    else {
        return resize_terminal_message(stdout);
    };
    let lyrics_height = if side_by_side {
        usable_height.saturating_sub(4)
    } else {
        usable_height.saturating_sub(left_rows.len() + 8)
    };
    let (right_title, mut right_rows, center_right) = if let Some(editor) = lyrics_editor {
        (editor.title(), editor.rows(show_lyrics), false)
    } else {
        (
            tr("msg.lyrics"),
            render_lyrics_panel_rows(lyrics, lyrics_width, lyrics_height, elapsed, lyrics_scroll),
            center_lyrics,
        )
    };
    if side_by_side {
        right_rows.truncate(usable_height.saturating_sub(4));
        let row_count = left_rows.len().max(right_rows.len());
        left_rows.resize(row_count, String::new());
        right_rows.resize(row_count, String::new());
    } else {
        right_rows.truncate(lyrics_height);
    }
    if !side_by_side && lyrics_height == 0 {
        return draw_panel(stdout, title, &left_rows);
    }
    let left = panel_lines(title, &left_rows, left_width, false);
    let right = panel_lines(right_title, &right_rows, lyrics_width, center_right);

    let frame = if side_by_side {
        (0..left.len())
            .map(|index| format!("{} {}", left[index], right[index]))
            .collect::<Vec<_>>()
            .join("\r\n")
    } else {
        format!("{}\r\n{}", left.join("\r\n"), right.join("\r\n"))
    };
    draw_frame(stdout, &frame)
}

pub(crate) fn draw_transcript_player_panels(
    stdout: &mut io::Stdout,
    title: &str,
    rows: &[String],
    transcript: Option<&Lyrics>,
    elapsed: Duration,
    show_transcript: bool,
    transcript_editor: Option<&TranscriptSettingsEditor>,
) -> Result<()> {
    let (terminal_columns, terminal_height) = terminal::size().unwrap_or((80, 24));
    if small_terminal(terminal_columns, terminal_height) {
        return resize_terminal_message(stdout);
    }
    let usable_height = usize::from(terminal_height.saturating_sub(1));
    let terminal_width = usize::from(terminal_columns);
    let left_width = player_panel_width();
    if !show_transcript && transcript_editor.is_none() {
        let Some(visible) = player_rows_for_height(rows, usable_height.saturating_sub(4)) else {
            return resize_terminal_message(stdout);
        };
        return draw_panel(stdout, title, &visible);
    }
    let prefer_side = load_setting_value("transcript.position").as_deref() != Some("below");
    let center_transcript = load_setting_value("transcript.alignment").as_deref() == Some("center");
    let minimum_right_width = if transcript_editor.is_some() { 38 } else { 24 };
    let side_by_side = prefer_side && terminal_width >= left_width + minimum_right_width + 10;
    let target_height = usable_height.saturating_sub(if side_by_side { 4 } else { 9 });
    let Some(mut left_rows) = player_rows_for_height(rows, target_height)
        .or_else(|| player_rows_for_height(rows, usable_height.saturating_sub(4)))
    else {
        return resize_terminal_message(stdout);
    };
    let transcript_height = if side_by_side {
        usable_height.saturating_sub(4)
    } else {
        usable_height.saturating_sub(left_rows.len() + 8)
    };
    let transcript_width = if side_by_side {
        terminal_width
            .saturating_sub(left_width + 10)
            .clamp(minimum_right_width, 60)
    } else {
        left_width
    };
    let (right_title, mut right_rows, center_right) = if let Some(editor) = transcript_editor {
        (editor.title(), editor.rows(show_transcript), false)
    } else {
        (
            tr("msg.transcript"),
            render_transcript_panel_rows(transcript, transcript_width, transcript_height, elapsed),
            center_transcript,
        )
    };
    if side_by_side {
        right_rows.truncate(usable_height.saturating_sub(4));
        let row_count = left_rows.len().max(right_rows.len());
        left_rows.resize(row_count, String::new());
        right_rows.resize(row_count, String::new());
    } else {
        right_rows.truncate(transcript_height);
    }
    if !side_by_side && transcript_height == 0 {
        return draw_panel(stdout, title, &left_rows);
    }
    let left = panel_lines(title, &left_rows, left_width, false);
    let right = panel_lines(right_title, &right_rows, transcript_width, center_right);

    let frame = if side_by_side {
        (0..left.len())
            .map(|index| format!("{} {}", left[index], right[index]))
            .collect::<Vec<_>>()
            .join("\r\n")
    } else {
        format!("{}\r\n{}", left.join("\r\n"), right.join("\r\n"))
    };
    draw_frame(stdout, &frame)
}

fn render_transcript_panel_rows(
    transcript: Option<&Lyrics>,
    width: usize,
    height: usize,
    elapsed: Duration,
) -> Vec<String> {
    let mut rows = match transcript {
        Some(transcript) => {
            let active_color = transcript_color_setting(TranscriptColorTarget::Active);
            let inactive_color = transcript_color_setting(TranscriptColorTarget::Inactive);
            let text_width = width.saturating_sub(2);
            transcript
                .render(text_width, height, elapsed, 0)
                .into_iter()
                .map(|line| {
                    let text = if line.bold_line {
                        bold_text(&line.text)
                    } else {
                        line.text
                    };
                    color_text(
                        &format!("{} {text}", if line.marker { ">" } else { " " }),
                        if line.active {
                            active_color
                        } else {
                            inactive_color
                        },
                    )
                })
                .collect::<Vec<_>>()
        }
        None => vec![tr("msg.no_transcript_available").to_string()],
    };
    rows.resize(height, String::new());
    rows
}

fn render_lyrics_panel_rows(
    lyrics: Option<&Lyrics>,
    width: usize,
    height: usize,
    elapsed: Duration,
    scroll: usize,
) -> Vec<String> {
    let mut rows = match lyrics {
        Some(lyrics) => {
            let content_height = if lyrics.is_plain() && height > 1 {
                height - 1
            } else {
                height
            };
            let timed = !lyrics.is_plain();
            let active_color = lyric_color_setting(LyricColorTarget::Active);
            let inactive_color = lyric_color_setting(LyricColorTarget::Inactive);
            let plain_color = lyric_color_setting(LyricColorTarget::Plain);
            let text_width = if timed {
                width.saturating_sub(2)
            } else {
                width
            };
            let mut rendered = lyrics
                .render(text_width, content_height, elapsed, scroll)
                .into_iter()
                .map(|line| {
                    let text = if let Some((start, end)) = line.active_word {
                        format!(
                            "{}{}{}",
                            &line.text[..start],
                            bold_text(&line.text[start..end]),
                            &line.text[end..]
                        )
                    } else if line.bold_line {
                        bold_text(&line.text)
                    } else {
                        line.text
                    };
                    if timed {
                        color_text(
                            &format!("{} {text}", if line.marker { ">" } else { " " }),
                            if line.active {
                                active_color
                            } else {
                                inactive_color
                            },
                        )
                    } else {
                        color_text(&text, plain_color)
                    }
                })
                .collect::<Vec<_>>();
            if lyrics.is_plain() && height > 1 {
                rendered.push(tr("msg.page_up_down_scroll").to_string());
            }
            rendered
        }
        None => vec![tr("msg.no_embedded_lyrics").to_string()],
    };
    rows.resize(height, String::new());
    rows.truncate(height);
    rows
}

fn panel_lines(title: &str, rows: &[String], width: usize, center_rows: bool) -> Vec<String> {
    let mut lines = Vec::with_capacity(rows.len() + 4);
    lines.push(format!("┌{}┐", "─".repeat(width + 2)));
    lines.push(format!("│ {} │", bold_text(&fit_text(title, width))));
    lines.push(format!("├{}┤", "─".repeat(width + 2)));
    lines.extend(rows.iter().map(|row| {
        let row = if center_rows {
            center_text(row, width)
        } else {
            fit_text(row, width)
        };
        format!("│ {row} │")
    }));
    lines.push(format!("└{}┘", "─".repeat(width + 2)));
    lines
}

fn visible_text_width(text: &str) -> usize {
    text.chars()
        .filter(|character| !is_style_marker(*character))
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn center_text(text: &str, width: usize) -> String {
    let fitted = fit_text(text, width);
    let content = fitted.trim_end_matches(' ');
    let content_width = visible_text_width(content);
    let left = width.saturating_sub(content_width) / 2;
    let right = width.saturating_sub(content_width + left);
    format!("{}{content}{}", " ".repeat(left), " ".repeat(right))
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
