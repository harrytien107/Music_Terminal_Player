use std::collections::HashSet;
use std::fs;
use std::hash::Hash;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use anyhow::Result;

use crate::i18n::tr;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

const BOLD_START: char = '\u{1}';
const BOLD_END: char = '\u{2}';

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
    fs::create_dir_all(DATA_DIR)?;
    fs::write(
        SETTINGS_FILE,
        format!("volume={:.2}\n", volume.clamp(0.0, MAX_VOLUME)),
    )?;
    Ok(())
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

pub(crate) fn menu_quit_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Char('q')
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
}

pub(crate) fn bold_text(text: &str) -> String {
    format!("{BOLD_START}{text}{BOLD_END}")
}

fn is_key_binding(token: &str) -> bool {
    matches!(
        token,
        "Ctrl+Space"
            | "Ctrl+A"
            | "Up/Down"
            | "Up/Down/Page Up/Page Down"
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
    )
}

pub(crate) fn bold_key_bindings(text: &str) -> String {
    const KEYS: &[&str] = &[
        "Up/Down/Page Up/Page Down",
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
        let mut frame = if title.is_empty() {
            String::new()
        } else {
            format!("{}\r\n\r\n", bold_menu_title(title))
        };
        for (index, label) in items.iter().enumerate() {
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
    const WIDTH: usize = 34;
    const PAGE_SIZE: usize = 12;
    let queued_start = current.saturating_add(1).min(queue.len());
    let queued_end = queued_start.saturating_add(play_next).min(queue.len());
    let remaining_start = queued_end;
    let queued_offset = queue_window_start(selected, queued_start, queued_end, PAGE_SIZE);
    let remaining_offset = queue_window_start(selected, remaining_start, queue.len(), PAGE_SIZE);
    let queued_visible_start = queued_start + queued_offset;
    let remaining_visible_start = remaining_start + remaining_offset;
    let queued = &queue[queued_visible_start..queued_end.min(queued_visible_start + PAGE_SIZE)];
    let remaining =
        &queue[remaining_visible_start..queue.len().min(remaining_visible_start + PAGE_SIZE)];
    let rows = queued.len().max(remaining.len()).max(1);
    let cell = |index: Option<usize>, item: Option<&T>| {
        item.map(|item| {
            fit_text(
                &format!(
                    "{} {}",
                    if index == Some(selected) { ">" } else { " " },
                    label(item)
                ),
                WIDTH,
            )
        })
        .unwrap_or_else(|| " ".repeat(WIDTH))
    };
    let mut frame = format!(
        "{} | {} {}\r\n\r\n┌{}┬{}┬{}┐\r\n│{}│{}│{}│\r\n├{}┼{}┼{}┤\r\n",
        tr("msg.playback_queue"),
        queue.len(),
        tr("msg.songs"),
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
        bold_text(&fit_text(tr("msg.now_playing"), WIDTH)),
        bold_text(&fit_text(tr("msg.next_in_queue"), WIDTH)),
        bold_text(&fit_text(tr("msg.next_from_track_list"), WIDTH)),
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
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
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
        "─".repeat(WIDTH),
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

pub(crate) fn playback_controls() -> Vec<String> {
    const CELL_WIDTH: usize = 14;
    let controls = [
        [
            tr("msg.p_play_pause"),
            tr("msg.v_previous"),
            tr("msg.n_next"),
        ],
        [tr("msg.r_shuffle"), tr("msg.l_loop"), tr("msg.u_queue")],
        [tr("msg.volume"), tr("msg.b_menu"), tr("msg.q_quit")],
    ];

    let border = |left, middle, right| {
        format!(
            "{left}{}{middle}{}{middle}{}{right}",
            "─".repeat(CELL_WIDTH + 2),
            "─".repeat(CELL_WIDTH + 2),
            "─".repeat(CELL_WIDTH + 2),
        )
    };
    let row = |cells: [&str; 3]| {
        format!(
            "│ {} │ {} │ {} │",
            fit_text(cells[0], CELL_WIDTH),
            fit_text(cells[1], CELL_WIDTH),
            fit_text(cells[2], CELL_WIDTH),
        )
    };

    vec![
        border('┌', '┬', '┐'),
        row(controls[0]),
        border('├', '┼', '┤'),
        row(controls[1]),
        border('├', '┼', '┤'),
        row(controls[2]),
        border('└', '┴', '┘'),
    ]
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
    if width == 0 {
        return String::new();
    }
    let visible_width = text
        .chars()
        .filter(|character| !matches!(*character, BOLD_START | BOLD_END))
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>();
    if visible_width <= width {
        return format!("{text}{}", " ".repeat(width - visible_width));
    }

    let content_width = width.saturating_sub(1);
    let mut value = String::new();
    let mut used = 0usize;
    let mut bold = false;
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
    value.push('…');
    value.push_str(&" ".repeat(width.saturating_sub(used + 1)));
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
            cursor::MoveToColumn(0),
            SetForegroundColor(NORMAL_TEXT_COLOR)
        )?;
        let line = line.trim_end_matches('\r');
        let line = if index == 0 && !line.contains(BOLD_START) {
            bold_text(line)
        } else {
            line.to_string()
        };
        let styled_line = bold_key_bindings(&line);
        for part in styled_line.split_inclusive([BOLD_START, BOLD_END]) {
            if let Some(text) = part.strip_suffix(BOLD_START) {
                output.write_all(text.as_bytes())?;
                queue!(output, SetAttribute(Attribute::Bold))?;
            } else if let Some(text) = part.strip_suffix(BOLD_END) {
                output.write_all(text.as_bytes())?;
                queue!(output, SetAttribute(Attribute::NormalIntensity))?;
            } else {
                output.write_all(part.as_bytes())?;
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
    let available =
        usize::from(terminal::size().map(|size| size.0).unwrap_or(80)).saturating_sub(4);
    let content = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.as_str()))
        .chain([UnicodeWidthStr::width(title)])
        .max()
        .unwrap_or(28);
    let width = content.clamp(28, 52).min(available.max(28));
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
