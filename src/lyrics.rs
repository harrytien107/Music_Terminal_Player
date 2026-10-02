use std::fs::File;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{Metadata, MetadataOptions, MetadataRevision, StandardTagKey, Value};
use symphonia::core::probe::Hint;
use unicode_width::UnicodeWidthChar;

const MAX_LYRICS_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimedLyricLine {
    pub(crate) at: Duration,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimedLyricToken {
    pub(crate) at: Duration,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Lyrics {
    Timed(Vec<TimedLyricLine>),
    Enhanced(Vec<TimedLyricToken>),
    Plain(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayLyricLine {
    pub(crate) text: String,
    pub(crate) active: bool,
    pub(crate) marker: bool,
    pub(crate) bold_line: bool,
    pub(crate) active_word: Option<(usize, usize)>,
}

impl Lyrics {
    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let normalized = normalize_lyrics(text);
        if normalized.is_empty() {
            return None;
        }
        if let Some(tokens) = parse_enhanced_lrc(&normalized) {
            return Some(Self::Enhanced(tokens));
        }
        if let Some(lines) = parse_lrc(&normalized) {
            return Some(Self::Timed(lines));
        }
        let lines = normalized.lines().map(str::to_string).collect::<Vec<_>>();
        (!lines.is_empty()).then_some(Self::Plain(lines))
    }

    pub(crate) fn is_plain(&self) -> bool {
        matches!(self, Self::Plain(_))
    }

    pub(crate) fn render(
        &self,
        width: usize,
        height: usize,
        elapsed: Duration,
        scroll: usize,
    ) -> Vec<DisplayLyricLine> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        match self {
            Self::Plain(lines) => {
                let wrapped = lines
                    .iter()
                    .flat_map(|line| wrap_line(line, width))
                    .collect::<Vec<_>>();
                let max_scroll = wrapped.len().saturating_sub(height);
                wrapped
                    .into_iter()
                    .skip(scroll.min(max_scroll))
                    .take(height)
                    .map(|text| DisplayLyricLine {
                        text,
                        active: false,
                        marker: false,
                        bold_line: false,
                        active_word: None,
                    })
                    .collect()
            }
            Self::Timed(lines) => {
                let active = lines.iter().rposition(|line| line.at <= elapsed);
                let wrapped = lines
                    .iter()
                    .enumerate()
                    .flat_map(|(line_index, line)| {
                        wrap_line(&line.text, width)
                            .into_iter()
                            .enumerate()
                            .map(move |(part_index, text)| (line_index, part_index, text))
                    })
                    .collect::<Vec<_>>();
                let active_row = active.and_then(|active| {
                    wrapped.iter().position(|(line_index, part_index, _)| {
                        *line_index == active && *part_index == 0
                    })
                });
                let start = centered_window_start(active_row.unwrap_or(0), wrapped.len(), height);
                wrapped
                    .into_iter()
                    .skip(start)
                    .take(height)
                    .map(|(line_index, part_index, text)| DisplayLyricLine {
                        text,
                        active: active == Some(line_index),
                        marker: active == Some(line_index) && part_index == 0,
                        bold_line: active == Some(line_index),
                        active_word: None,
                    })
                    .collect()
            }
            Self::Enhanced(tokens) => render_enhanced(tokens, width, height, elapsed),
        }
    }
}

fn centered_window_start(center: usize, total: usize, height: usize) -> usize {
    center
        .saturating_sub(height / 2)
        .min(total.saturating_sub(height))
}

fn render_enhanced(
    tokens: &[TimedLyricToken],
    width: usize,
    height: usize,
    elapsed: Duration,
) -> Vec<DisplayLyricLine> {
    let active_token = tokens.iter().rposition(|token| token.at <= elapsed);
    let rows = wrap_timed_tokens(tokens, width);
    let active_row = active_token.and_then(|active| {
        rows.iter()
            .position(|(_, row)| row.iter().any(|(token_index, _)| *token_index == active))
    });
    let active_line = active_row.map(|row| rows[row].0);
    let marker_row =
        active_line.and_then(|line| rows.iter().position(|(line_index, _)| *line_index == line));
    let start = centered_window_start(active_row.unwrap_or(0), rows.len(), height);

    rows.into_iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(row_index, (line_index, parts))| {
            let mut text = String::new();
            let mut active_word = None;
            for (token_index, part) in parts {
                let start = text.len();
                text.push_str(&part);
                if active_token == Some(token_index) {
                    let leading = part.len() - part.trim_start().len();
                    let trailing = part.trim_end().len();
                    if trailing > leading {
                        active_word = Some((start + leading, start + trailing));
                    }
                }
            }
            DisplayLyricLine {
                active: active_line == Some(line_index),
                marker: marker_row == Some(row_index),
                bold_line: false,
                active_word,
                text,
            }
        })
        .collect()
}

fn wrap_timed_tokens(
    tokens: &[TimedLyricToken],
    width: usize,
) -> Vec<(usize, Vec<(usize, String)>)> {
    let mut rows = Vec::new();
    let mut row = Vec::<(usize, String)>::new();
    let mut used = 0usize;
    let mut line_index = 0usize;

    for (token_index, token) in tokens.iter().enumerate() {
        for segment in token.text.split_inclusive('\n') {
            let content = segment.strip_suffix('\n').unwrap_or(segment);
            let segment_width = content
                .chars()
                .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                .sum::<usize>();
            if used > 0 && segment_width <= width && used + segment_width > width {
                finish_timed_row(&mut rows, &mut row, line_index);
                used = 0;
            }
            for character in content.chars() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if used > 0 && used + character_width > width {
                    finish_timed_row(&mut rows, &mut row, line_index);
                    used = 0;
                }
                if used == 0 && character.is_whitespace() {
                    continue;
                }
                if character_width > width {
                    continue;
                }
                if let Some((last_index, text)) = row.last_mut()
                    && *last_index == token_index
                {
                    text.push(character);
                } else {
                    row.push((token_index, character.to_string()));
                }
                used += character_width;
            }
            if segment.ends_with('\n') {
                finish_timed_row(&mut rows, &mut row, line_index);
                used = 0;
                line_index += 1;
            }
        }
    }
    finish_timed_row(&mut rows, &mut row, line_index);
    rows
}

fn finish_timed_row(
    rows: &mut Vec<(usize, Vec<(usize, String)>)>,
    row: &mut Vec<(usize, String)>,
    line_index: usize,
) {
    while let Some((_, text)) = row.last_mut() {
        let trimmed = text.trim_end_matches([' ', '\t']).len();
        text.truncate(trimmed);
        if text.is_empty() {
            row.pop();
        } else {
            break;
        }
    }
    if !row.is_empty() {
        rows.push((line_index, std::mem::take(row)));
    }
}

pub(crate) fn read_embedded_lyrics(path: &Path) -> Result<Option<Lyrics>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("failed to read metadata from {}", path.display()))?;

    let mut candidates = Vec::new();
    if let Some(mut metadata) = probed.metadata.get() {
        collect_metadata(&mut metadata, &mut candidates);
    }
    {
        let mut metadata = probed.format.metadata();
        collect_metadata(&mut metadata, &mut candidates);
    }
    let lyrics = candidates
        .into_iter()
        .filter_map(|text| Lyrics::from_text(&text).map(|lyrics| (text.len(), lyrics)))
        .max_by_key(|(length, _)| *length)
        .map(|(_, lyrics)| lyrics);
    Ok(lyrics)
}

fn collect_metadata(metadata: &mut Metadata<'_>, candidates: &mut Vec<String>) {
    loop {
        if let Some(revision) = metadata.current() {
            collect_revision(revision, candidates);
        }
        if metadata.is_latest() {
            break;
        }
        metadata.pop();
    }
}

fn collect_revision(revision: &MetadataRevision, candidates: &mut Vec<String>) {
    for tag in revision.tags() {
        let raw_key = tag
            .key
            .chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let is_lyrics = tag.std_key == Some(StandardTagKey::Lyrics)
            || matches!(
                raw_key.as_str(),
                "lyrics" | "lyric" | "unsyncedlyrics" | "unsynchronizedlyrics" | "uslt"
            );
        if is_lyrics && let Value::String(value) = &tag.value {
            candidates.push(truncate_lyrics(value));
        }
    }
}

fn truncate_lyrics(value: &str) -> String {
    if value.len() <= MAX_LYRICS_BYTES {
        return value.to_string();
    }
    let mut end = MAX_LYRICS_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn normalize_lyrics(text: &str) -> String {
    let cleaned = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect::<String>();
    let lines = cleaned.lines().map(str::trim_end).collect::<Vec<_>>();
    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].join("\n"),
        _ => String::new(),
    }
}

fn parse_lrc(text: &str) -> Option<Vec<TimedLyricLine>> {
    let mut lines = Vec::new();
    let mut offset_ms = 0i64;
    for source_line in text.lines() {
        let mut rest = source_line.trim_start();
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(close) = after_open.find(']') else {
                break;
            };
            let field = &after_open[..close];
            rest = &after_open[close + 1..];
            if let Some(offset) = field
                .strip_prefix("offset:")
                .or_else(|| field.strip_prefix("OFFSET:"))
                .and_then(|value| value.trim().parse::<i64>().ok())
            {
                offset_ms = offset;
            } else if let Some(timestamp) = parse_lrc_timestamp(field) {
                timestamps.push(timestamp);
            }
        }
        let lyric = rest.trim().to_string();
        for timestamp in timestamps {
            lines.push(TimedLyricLine {
                at: Duration::from_millis(timestamp),
                text: lyric.clone(),
            });
        }
    }
    if lines.is_empty() {
        return None;
    }
    for line in &mut lines {
        let adjusted = (line.at.as_millis() as i128 + offset_ms as i128).max(0) as u64;
        line.at = Duration::from_millis(adjusted);
    }
    lines.sort_by_key(|line| line.at);
    Some(lines)
}

fn parse_enhanced_lrc(text: &str) -> Option<Vec<TimedLyricToken>> {
    let mut prepared = String::new();
    let mut offset_ms = 0i64;
    for source_line in text.lines() {
        let mut rest = source_line.trim_start();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(close) = after_open.find(']') else {
                break;
            };
            let field = &after_open[..close];
            if let Some((key, value)) = field.split_once(':')
                && key.eq_ignore_ascii_case("offset")
                && let Ok(offset) = value.trim().parse::<i64>()
            {
                offset_ms = offset;
            }
            rest = &after_open[close + 1..];
        }
        prepared.push_str(rest);
        prepared.push('\n');
    }

    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    let mut current: Option<(u64, usize)> = None;
    while let Some(relative_open) = prepared[cursor..].find('<') {
        let open = cursor + relative_open;
        let Some(relative_close) = prepared[open + 1..].find('>') else {
            break;
        };
        let close = open + 1 + relative_close;
        let Some(timestamp) = parse_lrc_timestamp(&prepared[open + 1..close]) else {
            cursor = open + 1;
            continue;
        };
        if let Some((at, content_start)) = current.take() {
            push_enhanced_token(&mut tokens, at, &prepared[content_start..open]);
        }
        current = Some((timestamp, close + 1));
        cursor = close + 1;
    }
    if let Some((at, content_start)) = current {
        push_enhanced_token(&mut tokens, at, &prepared[content_start..]);
    }
    if tokens.is_empty() {
        return None;
    }
    for token in &mut tokens {
        let adjusted = (token.at.as_millis() as i128 + offset_ms as i128).max(0) as u64;
        token.at = Duration::from_millis(adjusted);
    }
    tokens.sort_by_key(|token| token.at);
    Some(tokens)
}

fn push_enhanced_token(tokens: &mut Vec<TimedLyricToken>, at: u64, text: &str) {
    if text.trim().is_empty() {
        if text.contains('\n')
            && let Some(previous) = tokens.last_mut()
            && !previous.text.ends_with('\n')
        {
            previous.text.push('\n');
        }
        return;
    }
    tokens.push(TimedLyricToken {
        at: Duration::from_millis(at),
        text: text.to_string(),
    });
}

fn parse_lrc_timestamp(value: &str) -> Option<u64> {
    let (minutes, seconds) = value.split_once(':')?;
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<f64>().ok()?;
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }
    Some(minutes * 60_000 + (seconds * 1000.0).round() as u64)
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;

    for word in line.split_whitespace() {
        let word_width = word
            .chars()
            .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
            .sum::<usize>();
        if word_width <= width {
            let separator = usize::from(!row.is_empty());
            if used + separator + word_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            if !row.is_empty() {
                row.push(' ');
                used += 1;
            }
            row.push_str(word);
            used += word_width;
            continue;
        }

        if !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        for character in word.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used > 0 && used + character_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            if character_width <= width {
                row.push(character);
                used += character_width;
            }
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_timestamps_offsets_and_multiple_markers() {
        let lyrics =
            Lyrics::from_text("[offset:100]\n[00:01.00][00:03.50]Hello\n[00:05.125]世界").unwrap();
        let Lyrics::Timed(lines) = lyrics else {
            panic!("expected timed lyrics");
        };
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].at, Duration::from_millis(1100));
        assert_eq!(lines[1].at, Duration::from_millis(3600));
        assert_eq!(lines[2].at, Duration::from_millis(5225));
        assert_eq!(lines[2].text, "世界");
    }

    #[test]
    fn timed_render_follows_elapsed_playback() {
        let lyrics = Lyrics::from_text("[00:01]one\n[00:02]two\n[00:03]three").unwrap();
        let rows = lyrics.render(20, 3, Duration::from_millis(2500), 0);
        assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
        assert!(rows.iter().any(|row| row.active && row.text == "two"));
    }

    #[test]
    fn timed_wrapped_active_line_marks_only_its_first_visual_row() {
        let lyrics = Lyrics::from_text("[00:01]one two three four five six\n[00:05]next").unwrap();
        let rows = lyrics.render(14, 4, Duration::from_millis(2500), 0);
        let active = rows.iter().filter(|row| row.active).collect::<Vec<_>>();
        assert_eq!(active.len(), 2);
        assert_eq!(active.iter().filter(|row| row.marker).count(), 1);
        assert!(active.iter().all(|row| row.bold_line));
        assert_eq!(active[0].text, "one two three");
        assert_eq!(active[1].text, "four five six");
    }

    #[test]
    fn enhanced_lrc_hides_angle_timestamps_and_tracks_the_current_word() {
        let lyrics = Lyrics::from_text(
            "<00:21.347>Floating <00:23.507>under <00:25.443>water\n\
             <00:26.855>Ever <00:28.812>changing <00:30.780>picture\n\
             <00:32.109>Hours <00:34.130>out <00:35.032>from <00:36.114>blue",
        )
        .unwrap();
        let Lyrics::Enhanced(tokens) = &lyrics else {
            panic!("expected enhanced lyrics");
        };
        assert_eq!(tokens.len(), 10);
        assert_eq!(tokens[0].at, Duration::from_millis(21_347));
        assert_eq!(tokens[0].text, "Floating ");
        assert!(tokens.iter().all(|token| !token.text.contains('<')));

        let rows = lyrics.render(28, 3, Duration::from_millis(29_000), 0);
        let active = rows.iter().find(|row| row.active).unwrap();
        assert_eq!(active.text, "Ever changing picture");
        let (start, end) = active.active_word.unwrap();
        assert_eq!(&active.text[start..end], "changing");
    }

    #[test]
    fn enhanced_wrapped_line_keeps_active_color_but_only_current_word_is_bold() {
        let lyrics = Lyrics::from_text(
            "<00:01>one <00:02>two <00:03>three <00:04>four <00:05>five <00:06>six\n\
             <00:10>next",
        )
        .unwrap();
        let rows = lyrics.render(13, 5, Duration::from_millis(4500), 0);
        let active = rows.iter().filter(|row| row.active).collect::<Vec<_>>();
        assert!(active.len() >= 2);
        assert_eq!(active.iter().filter(|row| row.marker).count(), 1);
        assert_eq!(
            active
                .iter()
                .filter(|row| row.active_word.is_some())
                .count(),
            1
        );
        assert!(active.iter().all(|row| !row.bold_line));
    }

    #[test]
    fn enhanced_lrc_accepts_line_timestamps_and_offsets() {
        let lyrics =
            Lyrics::from_text("[offset:100]\n[00:01.00]<00:01.00>Hello <00:01.50>world").unwrap();
        let Lyrics::Enhanced(tokens) = lyrics else {
            panic!("expected enhanced lyrics");
        };
        assert_eq!(tokens[0].at, Duration::from_millis(1100));
        assert_eq!(tokens[1].at, Duration::from_millis(1600));
        assert_eq!(tokens[0].text, "Hello ");
        assert_eq!(tokens[1].text, "world\n");
    }

    #[test]
    fn plain_lyrics_wrap_unicode_and_scroll() {
        let lyrics = Lyrics::from_text("abcdef\n世界世界").unwrap();
        let rows = lyrics.render(4, 2, Duration::ZERO, 1);
        assert_eq!(rows[0].text, "ef");
        assert_eq!(rows[1].text, "世界");
        assert!(rows.iter().all(|row| !row.active));
    }

    #[test]
    fn ordinary_words_move_whole_to_the_next_row() {
        let lyrics = Lyrics::from_text("This lyric has a beautiful ending").unwrap();
        let rows = lyrics.render(16, 4, Duration::ZERO, 0);
        assert_eq!(rows[0].text, "This lyric has a");
        assert_eq!(rows[1].text, "beautiful ending");
    }
}
