//! Optional, nonblocking Spicy Lyrics lookup for Local and Telegram playback.
//! Only publishable keys are accepted. API responses stay inside the application.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::lyrics::{Lyrics, TimedLyricLine, TimedLyricToken};
use crate::util::{DATA_DIR, load_setting_value};

const CACHE_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_REPLY: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LyricsSource {
    Embedded,
    Auto,
    Spicy,
}

impl LyricsSource {
    pub(crate) fn load() -> Self {
        Self::from_setting(load_setting_value("lyrics.source").as_deref())
    }

    fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("embedded") => Self::Embedded,
            Some("spicy") => Self::Spicy,
            _ => Self::Auto,
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Auto => "auto",
            Self::Spicy => "spicy",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Embedded => Self::Auto,
            Self::Auto => Self::Spicy,
            Self::Spicy => Self::Embedded,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TrackQuery {
    pub(crate) title: String,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) duration: Option<Duration>,
    pub(crate) local_path: Option<PathBuf>,
}

impl TrackQuery {
    pub(crate) fn from_name(name: &str, duration: Option<Duration>) -> Self {
        let stem = std::path::Path::new(name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(name)
            .trim();
        let (artist, title) = match stem.split_once(" - ") {
            Some((artist, title)) if !artist.trim().is_empty() && !title.trim().is_empty() => {
                (Some(artist.trim().to_string()), title.trim().to_string())
            }
            _ => (None, stem.to_string()),
        };
        Self {
            title,
            artist,
            album: None,
            duration,
            local_path: None,
        }
    }

    pub(crate) fn from_local(path: &std::path::Path, duration: Option<Duration>) -> Self {
        let mut query = Self::from_name(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
            duration,
        );
        query.local_path = Some(path.to_path_buf());
        query
    }
}

/// Keeps only the current track's asynchronous result; prior-track replies are discarded.
pub(crate) struct SpicyLyricsLookup {
    identity: String,
    last_key: Option<String>,
    receiver: Option<Receiver<Option<OnlineLyrics>>>,
    online: Option<OnlineLyrics>,
}

struct OnlineLyrics {
    lyrics: Lyrics,
    attribution: Vec<String>,
}

impl SpicyLyricsLookup {
    pub(crate) fn new() -> Self {
        Self {
            identity: String::new(),
            last_key: None,
            receiver: None,
            online: None,
        }
    }

    pub(crate) fn update(&mut self, identity: &str, query: TrackQuery, embedded: Option<&Lyrics>) {
        let source = LyricsSource::load();
        let new_identity = format!("{}:{identity}", source.key());
        let key = if source == LyricsSource::Embedded {
            None
        } else {
            publishable_key()
        };
        if self.identity != new_identity || self.last_key != key {
            self.identity = new_identity;
            self.last_key = key.clone();
            self.receiver = None;
            self.online = None;
            let needs_online = source != LyricsSource::Embedded
                && (source == LyricsSource::Spicy || embedded.is_none_or(Lyrics::is_plain));
            if needs_online && let Some(key) = key {
                let (sender, receiver) = mpsc::channel();
                self.receiver = Some(receiver);
                std::thread::spawn(move || {
                    let _ = sender.send(fetch(&query, &key));
                });
            }
        }
        if let Some(receiver) = self.receiver.as_ref() {
            match receiver.try_recv() {
                Ok(lyrics) => {
                    self.online = lyrics;
                    self.receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.receiver = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    pub(crate) fn display<'a>(&'a self, embedded: Option<&'a Lyrics>) -> Option<&'a Lyrics> {
        match LyricsSource::load() {
            LyricsSource::Embedded => embedded,
            LyricsSource::Auto if embedded.is_some_and(|lyrics| !lyrics.is_plain()) => embedded,
            LyricsSource::Auto | LyricsSource::Spicy => self
                .online
                .as_ref()
                .map(|result| &result.lyrics)
                .or(embedded),
        }
    }

    pub(crate) fn attribution(&self, embedded: Option<&Lyrics>) -> Option<&[String]> {
        match LyricsSource::load() {
            LyricsSource::Embedded => None,
            LyricsSource::Auto if embedded.is_some_and(|lyrics| !lyrics.is_plain()) => None,
            _ => self
                .online
                .as_ref()
                .map(|result| result.attribution.as_slice()),
        }
    }
}

pub(crate) const SPICY_KEY_SETTING: &str = "lyrics.spicylyrics_publishable_key";

pub(crate) fn valid_publishable_key(key: &str) -> bool {
    key.starts_with("sl_pk_")
        && key.len() < 512
        && key.len() > 8
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(crate) enum PublishableKeyOrigin {
    Saved,
    Environment,
    Missing,
}

pub(crate) fn publishable_key_origin() -> PublishableKeyOrigin {
    if crate::util::load_setting_value(SPICY_KEY_SETTING)
        .is_some_and(|value| valid_publishable_key(&value))
    {
        PublishableKeyOrigin::Saved
    } else if std::env::var("SPICY_LYRICS_PUBLISHABLE_KEY")
        .ok()
        .is_some_and(|value| valid_publishable_key(&value))
    {
        PublishableKeyOrigin::Environment
    } else {
        PublishableKeyOrigin::Missing
    }
}

fn publishable_key() -> Option<String> {
    crate::util::load_setting_value(SPICY_KEY_SETTING)
        .filter(|key| valid_publishable_key(key))
        .or_else(|| {
            std::env::var("SPICY_LYRICS_PUBLISHABLE_KEY")
                .ok()
                .filter(|key| valid_publishable_key(key))
        })
}

fn fetch(query: &TrackQuery, key: &str) -> Option<OnlineLyrics> {
    let mut query = query.clone();
    if let Some(path) = query.local_path.as_ref()
        && let Some((title, artist, album)) = crate::lyrics::read_track_tags(path)
    {
        if let Some(title) = title {
            query.title = title;
        }
        if let Some(artist) = artist {
            query.artist = Some(artist);
        }
        if let Some(album) = album {
            query.album = Some(album);
        }
    }
    if query.title.is_empty() {
        return None;
    }
    let cache_key = format!(
        "{}\0{}\0{}\0{}",
        query.title,
        query.artist.as_deref().unwrap_or(""),
        query.album.as_deref().unwrap_or(""),
        query.duration.map_or(0, |d| d.as_secs())
    );
    let name = format!("{:x}", Sha256::digest(cache_key.as_bytes()));
    let path = PathBuf::from(DATA_DIR)
        .join("spicylyrics-cache")
        .join(format!("{name}.json"));
    if let Some(body) = read_cache(&path) {
        return parse_lyrics(&body);
    }
    let mut params = vec![("title", query.title.as_str())];
    if let Some(artist) = &query.artist {
        params.push(("artist", artist));
    }
    if let Some(album) = &query.album {
        params.push(("album", album));
    }
    let duration_text = query
        .duration
        .map(|duration| format!("{:.3}", duration.as_secs_f64()));
    if let Some(duration) = duration_text.as_ref() {
        params.push(("duration", duration));
    }
    let url = format!(
        "https://api.spicylyrics.org/v1/tracks/search?{}",
        params
            .into_iter()
            .map(|(name, value)| format!("{name}={}", percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    );
    let response = request(&url, key)?;
    let result = choose_exact(&response, &query)?;
    let id = result.get("id")?.as_str()?;
    if id.len() != 22 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let body = request(&format!("https://api.spicylyrics.org/v1/lyrics/{id}"), key)?;
    let lyrics = parse_lyrics(&body)?;
    write_cache(&path, &body);
    Some(lyrics)
}

fn choose_exact<'a>(search: &'a Value, query: &TrackQuery) -> Option<&'a Value> {
    let result = search.get("Body")?.get("results")?.as_array()?.first()?;
    if result.get("tier")?.as_str()? != "exact" {
        return None;
    }
    if normalize(result.get("title")?.as_str()?) != normalize(&query.title) {
        return None;
    }
    if let Some(artist) = &query.artist {
        let artists = result.get("artists")?.as_array()?;
        if !artists
            .iter()
            .filter_map(Value::as_str)
            .any(|value| normalize(value) == normalize(artist))
        {
            return None;
        }
    }
    if let Some(duration) = query.duration {
        let actual = result.get("duration")?.as_f64()?;
        if (duration.as_secs_f64() - actual).abs() > 3.0 {
            return None;
        }
    }
    Some(result)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

/// Supply the bearer header via curl's stdin config, never its command line or logs.
fn request(url: &str, key: &str) -> Option<Value> {
    let mut child = Command::new("curl.exe")
        .args([
            "--silent",
            "--fail",
            "--show-error",
            "--max-time",
            "12",
            "--max-filesize",
            "2097152",
            "--config",
            "-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // The secret never appears in the command line, configuration file or diagnostic output.
    let mut stdin = child.stdin.take()?;
    let configuration = format!("header = \"Authorization: Bearer {key}\"\n");
    if stdin.write_all(configuration.as_bytes()).is_err() {
        let _ = child.kill();
        return None;
    }
    drop(stdin);
    let output = child.wait_with_output().ok()?;
    if !output.status.success() || output.stdout.len() > MAX_REPLY {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn cache_timestamp() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn cache_valid(now: u64, cached: u64) -> bool {
    cached <= now && now - cached < CACHE_LIFETIME.as_secs()
}

/// Remove stale responses, including those for tracks that are never played again.
/// This runs once per application start, independently of the selected lyrics source.
pub(crate) fn prune_expired_cache() {
    let Some(now) = cache_timestamp() else {
        return;
    };
    let root = PathBuf::from(DATA_DIR).join("spicylyrics-cache");
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let valid = fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() <= MAX_REPLY)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.get("cached_at")?.as_u64())
            .is_some_and(|timestamp| cache_valid(now, timestamp));
        if !valid {
            let _ = fs::remove_file(path);
        }
    }
}

fn read_cache(path: &PathBuf) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > MAX_REPLY {
        return None;
    }
    let entry: Value = serde_json::from_slice(&bytes).ok()?;
    if !cache_valid(cache_timestamp()?, entry.get("cached_at")?.as_u64()?) {
        let _ = fs::remove_file(path);
        return None;
    }
    entry.get("body").cloned()
}

fn write_cache(path: &PathBuf, body: &Value) {
    let Some(now) = cache_timestamp() else {
        return;
    };
    let entry = json!({"cached_at":now,"body":body});
    if let Ok(bytes) = serde_json::to_vec(&entry)
        && bytes.len() < MAX_REPLY
        && let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_ok()
    {
        let temp = path.with_extension("tmp");
        if fs::write(&temp, bytes).is_ok() {
            let _ = fs::rename(&temp, path);
        }
    }
}

fn timestamp(value: &Value) -> Option<Duration> {
    let value = value.as_f64()?;
    (value.is_finite() && (0.0..=86400.0).contains(&value)).then(|| Duration::from_secs_f64(value))
}

fn parse_lyrics(reply: &Value) -> Option<OnlineLyrics> {
    let body = reply.get("Body")?;
    let mut credits = vec![format!(
        "Lyrics: {}",
        match body.get("source").and_then(Value::as_str) {
            Some("spicy_lyrics") => "Spicy Lyrics",
            Some("apple_music") => "Apple Music",
            Some("spotify") => "Spotify",
            _ => "unknown source",
        }
    )];
    if body.get("source").and_then(Value::as_str) == Some("spicy_lyrics") {
        for (role, field) in [("Uploader", "Uploader"), ("Maker", "Maker")] {
            if let Some(author) = body
                .get("UploadAttribution")
                .and_then(|value| value.get(field))
            {
                let name = author.get("username")?.as_str()?;
                let url = author.get("url")?.as_str()?;
                if !url.starts_with("https://") {
                    return None;
                }
                credits.push(format!("{role}: {name}"));
            } else if role == "Uploader" {
                return None;
            }
        }
    }
    let content = body.get("Content")?.as_array()?;
    let lyrics = match body.get("Type")?.as_str()? {
        "Syllable" => {
            let mut tokens = Vec::new();
            for line in content {
                let Some(syllables) = line
                    .get("Lead")
                    .and_then(|lead| lead.get("Syllables"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                let start = tokens.len();
                for syllable in syllables {
                    let (Some(text), Some(at)) = (
                        syllable.get("Text").and_then(Value::as_str),
                        syllable.get("StartTime").and_then(timestamp),
                    ) else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(last) = tokens.last_mut() {
                        let last: &mut TimedLyricToken = last;
                        if !last.text.ends_with(char::is_whitespace)
                            && !text.starts_with(char::is_whitespace)
                            && !syllable
                                .get("IsPartOfWord")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        {
                            last.text.push(' ');
                        }
                    }
                    tokens.push(TimedLyricToken {
                        at,
                        text: text.to_string(),
                    });
                }
                if tokens.len() > start
                    && let Some(last) = tokens.last_mut()
                {
                    last.text.push('\n');
                }
            }
            if tokens.is_empty() {
                return None;
            }
            Lyrics::Enhanced(tokens)
        }
        "Line" => {
            let mut lines = Vec::new();
            for line in content {
                let entry = line.get("Lead").unwrap_or(line);
                if let (Some(text), Some(at)) = (
                    entry.get("Text").and_then(Value::as_str),
                    entry.get("StartTime").and_then(timestamp),
                ) {
                    lines.push(TimedLyricLine {
                        at,
                        text: text.to_string(),
                    });
                }
            }
            if lines.is_empty() {
                return None;
            }
            lines.sort_by_key(|line| line.at);
            Lyrics::Timed(lines)
        }
        "Static" => {
            let lines = content
                .iter()
                .filter_map(|line| {
                    line.as_str()
                        .or_else(|| line.get("Text").and_then(Value::as_str))
                        .or_else(|| {
                            line.get("Lead")
                                .and_then(|lead| lead.get("Text"))
                                .and_then(Value::as_str)
                        })
                })
                .map(str::to_string)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                return None;
            }
            Lyrics::Plain(lines)
        }
        _ => return None,
    };
    Some(OnlineLyrics {
        lyrics,
        attribution: credits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyrics_source_defaults_to_auto_and_preserves_saved_choices() {
        assert_eq!(LyricsSource::from_setting(None), LyricsSource::Auto);
        assert_eq!(LyricsSource::from_setting(Some("auto")), LyricsSource::Auto);
        assert_eq!(
            LyricsSource::from_setting(Some("embedded")),
            LyricsSource::Embedded
        );
        assert_eq!(
            LyricsSource::from_setting(Some("spicy")),
            LyricsSource::Spicy
        );
    }

    #[test]
    fn validate_only_publishable_client_keys() {
        assert!(valid_publishable_key("sl_pk_abc-123_XYZ"));
        assert!(!valid_publishable_key("sl_sk_abc-123_XYZ"));
        assert!(!valid_publishable_key("sl_pk_"));
        assert!(!valid_publishable_key("sl_pk_a"));
        assert!(!valid_publishable_key("sl_pk_abc\nmore"));
        assert!(!valid_publishable_key("sl_pk_abc=def"));
        assert!(!valid_publishable_key(&format!(
            "sl_pk_{}",
            "a".repeat(506)
        )));
    }

    #[test]
    fn reject_non_exact_versions_and_wrong_duration() {
        let query = TrackQuery {
            title: "Hello".into(),
            artist: Some("Artist".into()),
            album: None,
            duration: Some(Duration::from_secs(200)),
            local_path: None,
        };
        let search = json!({"Body":{"results":[{"tier":"version","title":"Hello","artists":["Artist"],"duration":200.0,"id":"1234567890123456789012"}]}});
        assert!(choose_exact(&search, &query).is_none());
        let search = json!({"Body":{"results":[{"tier":"exact","title":"Hello","artists":["Artist"],"duration":240.0,"id":"1234567890123456789012"}]}});
        assert!(choose_exact(&search, &query).is_none());
        let search = json!({"Body":{"results":[{"tier":"exact","title":"Hello","artists":["Artist"],"duration":200.0,"id":"1234567890123456789012"}]}});
        assert!(choose_exact(&search, &query).is_some());
    }

    #[test]
    fn parse_syllables_lines_static_and_attribution() {
        let credits = json!({"Uploader":{"username":"iPixelGalaxy","url":"https://spicylyrics.org/uid/1"},"Maker":{"username":"ItsNoot3rch <3","url":"https://spicylyrics.org/uid/2"}});
        let response = json!({"Body":{"source":"spicy_lyrics","UploadAttribution":credits,"Type":"Syllable","Content":[{"Lead":{"Syllables":[{"Text":"Hello","StartTime":1.0},{"Text":"world","StartTime":1.4}]}}]}});
        let mut invalid_url_response = response.clone();
        invalid_url_response["Body"]["UploadAttribution"]["Uploader"]["url"] =
            json!("http://spicylyrics.org/uid/1");
        assert!(parse_lyrics(&invalid_url_response).is_none());
        let Some(OnlineLyrics {
            lyrics: Lyrics::Enhanced(tokens),
            attribution,
        }) = parse_lyrics(&response)
        else {
            panic!("expected enhanced")
        };
        assert_eq!(
            attribution,
            [
                "Lyrics: Spicy Lyrics",
                "Uploader: iPixelGalaxy",
                "Maker: ItsNoot3rch <3"
            ]
        );
        assert!(
            attribution
                .iter()
                .all(|line| crate::lyrics::wrap_line(line, 35).len() == 1)
        );
        assert_eq!(tokens[0].at, Duration::from_secs(1));
        let response = json!({"Body":{"source":"spotify","Type":"Line","Content":[{"Text":"Line one","StartTime":2.0}]}});
        assert!(matches!(
            parse_lyrics(&response).map(|result| result.lyrics),
            Some(Lyrics::Timed(_))
        ));
        let response = json!({"Body":{"source":"apple_music","Type":"Static","Content":[{"Text":"Plain line"}]}});
        assert!(matches!(
            parse_lyrics(&response).map(|result| result.lyrics),
            Some(Lyrics::Plain(_))
        ));
    }

    #[test]
    fn cache_expiration_and_offline_fallback() {
        let now = 5_000_000;
        assert!(cache_valid(now, now));
        assert!(!cache_valid(now, now - CACHE_LIFETIME.as_secs()));
        assert!(!cache_valid(now, now + 1));
        let lookup = SpicyLyricsLookup::new();
        let embedded = Lyrics::Plain(vec!["Existing".into()]);
        // Without an API key, embedded lyrics remain usable even with Auto selected.
        assert!(lookup.online.is_none());
        assert!(embedded.is_plain());
    }
}
