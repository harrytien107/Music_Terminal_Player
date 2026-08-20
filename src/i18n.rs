use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{OnceLock, RwLock};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::util::DATA_DIR;

const LANGUAGE_FILE: &str = ".music-terminal/language.txt";
const LANGUAGE_DIR: &str = ".music-terminal/languages";
const CATALOG_URL: &str = "https://raw.githubusercontent.com/harrytien107/Music_Terminal_Player/main/languages/index.json";
const PACK_BASE_URL: &str =
    "https://raw.githubusercontent.com/harrytien107/Music_Terminal_Player/main/languages/";
const MAX_PACK_BYTES: u64 = 1024 * 1024;
const ENGLISH_PACK: &str = include_str!("../languages/en.lang");

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);
static ENGLISH: OnceLock<LanguagePack> = OnceLock::new();
static OPTIONAL: OnceLock<RwLock<HashMap<Language, LanguagePack>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Language {
    English,
    Vietnamese,
}

impl Language {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Vietnamese => "vi",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Vietnamese => "Tiếng Việt",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::English => "en.lang",
            Self::Vietnamese => "vi.lang",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageInfo {
    pub(crate) language: Language,
    pub(crate) name: String,
    pub(crate) version: u64,
    pub(crate) file: String,
    pub(crate) sha256: String,
}

struct LanguagePack {
    name: String,
    version: u64,
    messages: HashMap<String, &'static str>,
}

pub(crate) fn parse_language(text: &str) -> Language {
    match text.trim().to_ascii_lowercase().as_str() {
        "vi" => Language::Vietnamese,
        _ => Language::English,
    }
}

pub(crate) fn load_language() {
    let selected = load_language_from(Path::new(LANGUAGE_FILE));
    if selected == Language::Vietnamese && load_installed_pack(selected).is_ok() {
        LANGUAGE.store(selected as u8, Ordering::Relaxed);
    } else {
        LANGUAGE.store(Language::English as u8, Ordering::Relaxed);
    }
}

pub(crate) fn load_language_from(path: &Path) -> Language {
    fs::read_to_string(path)
        .map(|text| parse_language(&text))
        .unwrap_or(Language::English)
}

pub(crate) fn language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        value if value == Language::Vietnamese as u8 => Language::Vietnamese,
        _ => Language::English,
    }
}

pub(crate) fn set_language(language: Language) -> Result<()> {
    if language != Language::English {
        load_installed_pack(language)?;
    }
    fs::create_dir_all(DATA_DIR)?;
    save_language_to(Path::new(LANGUAGE_FILE), language)?;
    LANGUAGE.store(language as u8, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn save_language_to(path: &Path, language: Language) -> Result<()> {
    fs::write(path, format!("{}\n", language.code()))?;
    Ok(())
}

pub(crate) fn tr(key: &'static str) -> &'static str {
    if language() != Language::English
        && let Ok(packs) = optional_packs().read()
        && let Some(value) = packs
            .get(&language())
            .and_then(|pack| pack.messages.get(key))
    {
        return value;
    }
    english_pack().messages.get(key).copied().unwrap_or(key)
}

pub(crate) fn installed_language_version(language: Language) -> Option<u64> {
    if language == Language::English {
        return Some(english_pack().version);
    }
    let path = language_path(language);
    let text = fs::read_to_string(path).ok()?;
    parse_pack(&text, Some(language.code()))
        .ok()
        .map(|pack| pack.version)
}

pub(crate) fn fetch_language_catalog() -> Result<Vec<LanguageInfo>> {
    let output = Command::new("curl.exe")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--max-filesize",
            "1048576",
            "--max-time",
            "10",
            CATALOG_URL,
        ])
        .output()
        .context("failed to check languages with Windows curl.exe")?;
    if !output.status.success() {
        bail!("could not download the language catalog");
    }
    if output.stdout.len() as u64 > MAX_PACK_BYTES {
        bail!("language catalog is larger than 1 MiB");
    }
    parse_language_catalog(
        &String::from_utf8(output.stdout).context("language catalog is not UTF-8")?,
    )
}

pub(crate) fn install_language(info: &LanguageInfo) -> Result<()> {
    if info.language == Language::English {
        return Ok(());
    }
    validate_pack_file_name(&info.file)?;
    fs::create_dir_all(LANGUAGE_DIR)?;
    let destination = language_path(info.language);
    let temporary = destination.with_extension("lang.download");
    let url = format!("{PACK_BASE_URL}{}", info.file);
    let status = Command::new("curl.exe")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--max-filesize",
            "1048576",
            "--max-time",
            "30",
            "--output",
        ])
        .arg(&temporary)
        .arg(&url)
        .status()
        .context("failed to download the language pack with Windows curl.exe")?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        bail!("language-pack download failed: {url}");
    }
    let result = (|| {
        let metadata = fs::metadata(&temporary)?;
        if metadata.len() > MAX_PACK_BYTES {
            bail!("language pack is larger than 1 MiB");
        }
        let bytes = fs::read(&temporary)?;
        let actual_hash = format!("{:x}", Sha256::digest(&bytes));
        if !actual_hash.eq_ignore_ascii_case(&info.sha256) {
            bail!("language-pack SHA-256 does not match the catalog");
        }
        let text = String::from_utf8(bytes).context("language pack is not UTF-8")?;
        let pack = parse_pack(&text, Some(info.language.code()))?;
        if pack.version != info.version || pack.name != info.name {
            bail!("language-pack metadata does not match the catalog");
        }
        replace_file(&temporary, &destination)?;
        optional_packs()
            .write()
            .map_err(|_| anyhow::anyhow!("language-pack lock poisoned"))?
            .insert(info.language, pack);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn parse_language_catalog(text: &str) -> Result<Vec<LanguageInfo>> {
    let root: Value = serde_json::from_str(text).context("invalid language catalog JSON")?;
    if root.get("format").and_then(Value::as_u64) != Some(1) {
        bail!("unsupported language catalog format");
    }
    let entries = root
        .get("languages")
        .and_then(Value::as_array)
        .context("language catalog has no languages array")?;
    let mut seen = HashSet::new();
    let mut languages = Vec::new();
    for entry in entries {
        let code = entry.get("code").and_then(Value::as_str).unwrap_or("");
        let language = match code {
            "vi" => Language::Vietnamese,
            "en" => Language::English,
            _ => continue,
        };
        if !seen.insert(language) {
            bail!("language catalog contains duplicate code {code}");
        }
        let name = required_catalog_string(entry, "name")?.to_string();
        let version = entry
            .get("version")
            .and_then(Value::as_u64)
            .context("language catalog entry has no valid version")?;
        let file = required_catalog_string(entry, "file")?.to_string();
        validate_pack_file_name(&file)?;
        if file != language.file_name() {
            bail!("language catalog file does not match code {code}");
        }
        let sha256 = required_catalog_string(entry, "sha256")?.to_ascii_lowercase();
        if sha256.len() != 64
            || !sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            bail!("language catalog entry has an invalid SHA-256");
        }
        languages.push(LanguageInfo {
            language,
            name,
            version,
            file,
            sha256,
        });
    }
    Ok(languages)
}

pub(crate) fn parse_pack_value(value: &str) -> Result<String> {
    let mut decoded = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters
            .next()
            .context("language-pack value ends with an escape")?
        {
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '\\' => decoded.push('\\'),
            other => bail!("unsupported language-pack escape: \\{other}"),
        }
    }
    if decoded
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        bail!("language-pack value contains a terminal control character");
    }
    Ok(decoded)
}

#[cfg(test)]
pub(crate) fn validate_language_pack(text: &str, expected_code: &str) -> Result<()> {
    parse_pack(text, Some(expected_code)).map(|_| ())
}

fn english_pack() -> &'static LanguagePack {
    ENGLISH.get_or_init(|| {
        parse_pack(ENGLISH_PACK, Some("en")).expect("embedded English pack is invalid")
    })
}

fn optional_packs() -> &'static RwLock<HashMap<Language, LanguagePack>> {
    OPTIONAL.get_or_init(|| RwLock::new(HashMap::new()))
}

fn load_installed_pack(language: Language) -> Result<()> {
    if language == Language::English {
        return Ok(());
    }
    if optional_packs()
        .read()
        .map_err(|_| anyhow::anyhow!("language-pack lock poisoned"))?
        .contains_key(&language)
    {
        return Ok(());
    }
    let path = language_path(language);
    let metadata =
        fs::metadata(&path).with_context(|| format!("{} is not installed", language.label()))?;
    if metadata.len() > MAX_PACK_BYTES {
        bail!("installed language pack is larger than 1 MiB");
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let pack = parse_pack(&text, Some(language.code()))?;
    optional_packs()
        .write()
        .map_err(|_| anyhow::anyhow!("language-pack lock poisoned"))?
        .insert(language, pack);
    Ok(())
}

fn parse_pack(text: &str, expected_code: Option<&str>) -> Result<LanguagePack> {
    let mut fields = HashMap::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("language-pack line {} has no =", index + 1))?;
        if key.is_empty()
            || !key.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_')
            })
        {
            bail!("language-pack line {} has an invalid key", index + 1);
        }
        if fields
            .insert(key.to_string(), parse_pack_value(value)?)
            .is_some()
        {
            bail!("language pack contains duplicate key {key}");
        }
    }
    let code = fields
        .remove("language.code")
        .context("language pack has no language.code")?;
    if expected_code.is_some_and(|expected| expected != code) {
        bail!("language pack code does not match {expected_code:?}");
    }
    let name = fields
        .remove("language.name")
        .context("language pack has no language.name")?;
    let version = fields
        .remove("language.version")
        .context("language pack has no language.version")?
        .parse()
        .context("language pack has an invalid language.version")?;
    let messages = fields
        .into_iter()
        .filter(|(key, _)| key.starts_with("msg."))
        // ponytail: Pack updates are rare; leaked strings keep tr() allocation-free. Use Arc<str> if live reload becomes frequent.
        .map(|(key, value)| (key, Box::leak(value.into_boxed_str()) as &'static str))
        .collect();
    Ok(LanguagePack {
        name,
        version,
        messages,
    })
}

fn required_catalog_string<'a>(entry: &'a Value, key: &str) -> Result<&'a str> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("language catalog entry has no {key}"))
}

fn validate_pack_file_name(file: &str) -> Result<()> {
    let path = Path::new(file);
    if path.components().count() != 1
        || path.extension().and_then(|extension| extension.to_str()) != Some("lang")
    {
        bail!("invalid language-pack file name");
    }
    Ok(())
}

fn language_path(language: Language) -> PathBuf {
    Path::new(LANGUAGE_DIR).join(language.file_name())
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    let backup = destination.with_extension("lang.backup");
    let _ = fs::remove_file(&backup);
    if destination.exists() {
        fs::rename(destination, &backup)?;
    }
    match fs::rename(temporary, destination) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            if backup.exists() {
                let _ = fs::rename(backup, destination);
            }
            Err(error.into())
        }
    }
}
