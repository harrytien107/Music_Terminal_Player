use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::util::DATA_DIR;

const LATEST: &str =
    "https://api.github.com/repos/harrytien107/Music_Terminal_Player/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
struct ZipAsset {
    name: String,
    url: String,
    digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseInfo {
    pub(crate) tag: String,
    asset: Option<ZipAsset>,
}

#[cfg(test)]
impl ReleaseInfo {
    pub(crate) fn without_asset(tag: &str) -> Self {
        Self {
            tag: tag.into(),
            asset: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateState {
    Unchecked,
    Checking,
    UpToDate(String),
    Available(ReleaseInfo, Option<String>),
    Error(String),
    Downloading(String),
    Staged(String),
}

impl UpdateState {
    pub(crate) fn can_install(&self) -> bool {
        matches!(self, Self::Available(info, _) if info.asset.is_some())
    }
}

static UPDATE_STATE: OnceLock<Mutex<UpdateState>> = OnceLock::new();

fn state() -> &'static Mutex<UpdateState> {
    UPDATE_STATE.get_or_init(|| Mutex::new(UpdateState::Unchecked))
}

pub(crate) fn snapshot() -> UpdateState {
    state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

fn set_state(value: UpdateState) {
    *state().lock().unwrap_or_else(|poison| poison.into_inner()) = value;
}

pub(crate) fn start_version_check() {
    {
        let mut current = state().lock().unwrap_or_else(|poison| poison.into_inner());
        if matches!(
            *current,
            UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Staged(_)
        ) {
            return;
        }
        *current = UpdateState::Checking;
    }
    thread::spawn(|| {
        let result =
            check_latest_release().unwrap_or_else(|error| UpdateState::Error(format!("{error:#}")));
        set_state(result);
    });
}

pub(crate) fn start_manual_update() -> bool {
    let release = {
        let mut current = state().lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(release) = take_installable_release(&mut current) else {
            return false;
        };
        release
    };
    thread::spawn(move || {
        let next = match stage_release_update(&release) {
            Ok(()) => UpdateState::Staged(release.tag.clone()),
            Err(error) => {
                let message = format!("{error:#}");
                let _ = fs::write(
                    Path::new(DATA_DIR).join("update-status.txt"),
                    format!("Update: {message}"),
                );
                UpdateState::Available(release, Some(message))
            }
        };
        set_state(next);
    });
    true
}

fn take_installable_release(current: &mut UpdateState) -> Option<ReleaseInfo> {
    let UpdateState::Available(release, _) = current else {
        return None;
    };
    if release.asset.is_none() {
        return None;
    }
    let release = release.clone();
    *current = UpdateState::Downloading(release.tag.clone());
    Some(release)
}

fn version_numbers(version: &str) -> Option<[u32; 3]> {
    let text = version.trim().trim_start_matches('v');
    let text = text.split_once('-').map_or(text, |(numbers, _)| numbers);
    let parts = text
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (parts.len() == 3).then(|| [parts[0], parts[1], parts[2]])
}

fn newer_release(tag: &str, installed: &str) -> bool {
    version_numbers(tag)
        .zip(version_numbers(installed))
        .is_some_and(|(new, old)| new > old)
}

fn select_zip_asset(release: &Value) -> Option<(&str, &str, Option<&str>)> {
    let assets = release.get("assets")?.as_array()?;
    let eligible = assets
        .iter()
        .filter(|asset| {
            let name = asset
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            name.ends_with(".zip")
                && !name.contains("source")
                && !name.contains("linux")
                && !name.contains("macos")
                && !name.contains("darwin")
                && !name.contains("android")
                && !name.contains("arm")
                && (!name.contains("x86") || name.contains("x86_64"))
        })
        .collect::<Vec<_>>();
    let is_windows = |asset: &&Value| {
        let name = asset
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        name.contains("windows")
            || name.contains("win64")
            || name.contains("win-x64")
            || name.contains("win-x86_64")
    };
    let asset = eligible
        .iter()
        .copied()
        .find(is_windows)
        .or_else(|| (eligible.len() == 1).then(|| eligible[0]))?;
    Some((
        asset.get("name")?.as_str()?,
        asset.get("browser_download_url")?.as_str()?,
        asset.get("digest").and_then(Value::as_str),
    ))
}

fn curl_output(url: &str, dest: Option<&Path>) -> Result<Vec<u8>> {
    if !url.starts_with("https://") {
        bail!("update URL must use HTTPS");
    }
    let mut command = Command::new("curl.exe");
    command.args([
        "-fLsS",
        "--retry",
        "2",
        "--connect-timeout",
        "10",
        "--max-time",
        "180",
        "--max-filesize",
        "250000000",
        "-H",
        "User-Agent: music-terminal-player",
    ]);
    if let Some(dest) = dest {
        command.arg("-o").arg(dest);
    }
    let output = command
        .arg(url)
        .output()
        .context("curl.exe is required for updates")?;
    if !output.status.success() {
        bail!(
            "GitHub request failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn verify_digest(path: &Path, digest: Option<&str>) -> Result<()> {
    let Some(expected) = digest.and_then(|digest| digest.strip_prefix("sha256:")) else {
        return Ok(());
    };
    let bytes = fs::read(path)?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("GitHub release ZIP checksum mismatch");
    }
    Ok(())
}

fn valid_windows_x64_executable(bytes: &[u8]) -> bool {
    if bytes.len() < 1024 * 1024 || !bytes.starts_with(b"MZ") {
        return false;
    }
    let Some(offset) = bytes.get(0x3c..0x40) else {
        return false;
    };
    let offset = u32::from_le_bytes(offset.try_into().unwrap()) as usize;
    let Some(header) = bytes.get(offset..offset.saturating_add(6)) else {
        return false;
    };
    header.starts_with(b"PE\0\0") && header[4..6] == [0x64, 0x86]
}

fn check_latest_release() -> Result<UpdateState> {
    let response = curl_output(LATEST, None)?;
    let release: Value =
        serde_json::from_slice(&response).context("invalid GitHub release response")?;
    classify_release(&release, env!("CARGO_PKG_VERSION"))
}

fn classify_release(release: &Value, installed: &str) -> Result<UpdateState> {
    if release.get("draft").and_then(Value::as_bool) == Some(true)
        || release.get("prerelease").and_then(Value::as_bool) == Some(true)
    {
        bail!("latest GitHub release is not stable");
    }
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .context("GitHub release missing version")?;
    if version_numbers(tag).is_none() {
        bail!("latest GitHub release has an invalid version");
    }
    if !newer_release(tag, installed) {
        return Ok(UpdateState::UpToDate(tag.to_string()));
    }
    let asset = select_zip_asset(&release)
        .map(|(name, url, digest)| ZipAsset {
            name: name.to_string(),
            url: url.to_string(),
            digest: digest.map(str::to_string),
        })
        .filter(|asset| {
            asset.url.starts_with(
                "https://github.com/harrytien107/Music_Terminal_Player/releases/download/",
            )
        });
    Ok(UpdateState::Available(
        ReleaseInfo {
            tag: tag.to_string(),
            asset,
        },
        None,
    ))
}

fn stage_release_update(release: &ReleaseInfo) -> Result<()> {
    let asset = release
        .asset
        .as_ref()
        .context("release has no Windows ZIP asset")?;
    if !asset
        .url
        .starts_with("https://github.com/harrytien107/Music_Terminal_Player/releases/download/")
    {
        bail!("unexpected GitHub release download URL");
    }
    let current = std::env::current_exe()?;
    if current.file_name().and_then(|name| name.to_str()) != Some("music-terminal-player.exe") {
        bail!("updates require the installed music-terminal-player.exe");
    }
    let parent = current.parent().context("EXE has no parent directory")?;
    let stage = parent.join(DATA_DIR).join("app-update");
    fs::create_dir_all(&stage)?;
    let archive = stage.join("release.zip");
    curl_output(&asset.url, Some(&archive))
        .with_context(|| format!("cannot download release {}", asset.name))?;
    verify_digest(&archive, asset.digest.as_deref())?;
    let candidate = stage.join("music-terminal-player.new.exe");
    extract_release_exe(&archive, &candidate, &stage)?;
    let binary = fs::read(&candidate)?;
    if !valid_windows_x64_executable(&binary) {
        bail!("release ZIP contains an invalid Windows x64 executable");
    }
    let _ = fs::remove_file(&archive);
    fs::write(
        Path::new(DATA_DIR).join("update-status.txt"),
        format!("Update {} staged; will install after exit", release.tag),
    )?;
    install_after_exit(&current, &candidate, &stage, &std::env::current_dir()?)?;
    Ok(())
}

fn extract_release_exe(archive: &Path, target: &Path, stage: &Path) -> Result<()> {
    // Read the archive using Windows .NET; extract exactly one expected EXE, with
    // no traversal, symlinks, extra binaries or writes outside the staging folder.
    const EXTRACT: &str = r#"
param([string]$Archive, [string]$Target)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::OpenRead($Archive)
try {
  $matches = @($zip.Entries | Where-Object {
    $parts = $_.FullName.Replace('\','/').Split('/')
    $parts.Length -le 2 -and $parts[-1] -ieq 'music-terminal-player.exe' -and
      !($_.FullName.StartsWith('/')) -and !($_.FullName.Contains(':')) -and
      !($parts -contains '..') -and $_.Length -gt 1048576 -and $_.Length -lt 209715200
  })
  if ($matches.Count -ne 1) { throw 'Expected exactly one Windows EXE at ZIP root or in one folder' }
  $source = $matches[0].Open()
  try {
    $dest = [System.IO.File]::Create($Target)
    try { $source.CopyTo($dest) } finally { $dest.Dispose() }
  } finally { $source.Dispose() }
} finally { $zip.Dispose() }
"#;
    let script = stage.join("extract.ps1");
    fs::write(&script, EXTRACT)?;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-Archive")
        .arg(archive)
        .arg("-Target")
        .arg(target)
        .output()
        .context("PowerShell ZIP extraction failed")?;
    if !output.status.success() {
        bail!(
            "ZIP extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

// The detached worker waits for the running EXE to close before replacing it.
#[cfg(windows)]
const INSTALL_SCRIPT: &str = r#"
param([int]$ParentId,[string]$Exe,[string]$Staged,[string]$Backup,[string]$WorkDir,[string]$Status,[string]$WindowMode)
$ErrorActionPreference = 'Stop'
try { Wait-Process -Id $ParentId -ErrorAction SilentlyContinue } catch {}
$lastError = 'File remained locked'
$backupReady = $false
for ($attempt = 0; $attempt -lt 20; $attempt++) {
  try {
    Copy-Item -LiteralPath $Exe -Destination $Backup -Force -ErrorAction Stop
    $backupReady = $true
    break
  } catch {
    $lastError = $_.Exception.Message
    Start-Sleep -Milliseconds 500
  }
}
if (!$backupReady) {
  Set-Content -LiteralPath $Status -Value "Update failed while backing up old EXE: $lastError" -Encoding UTF8
  exit 1
}
for ($attempt = 0; $attempt -lt 20; $attempt++) {
  try {
    Copy-Item -LiteralPath $Staged -Destination $Exe -Force -ErrorAction Stop
    if ($WindowMode -eq 'borderless') {
      Start-Process -FilePath $Exe -WorkingDirectory $WorkDir -ArgumentList '--borderless' -ErrorAction Stop
    } else {
      Start-Process -FilePath $Exe -WorkingDirectory $WorkDir -ErrorAction Stop
    }
    try { Set-Content -LiteralPath $Status -Value 'Update installed successfully' -Encoding UTF8 } catch {}
    exit 0
  } catch {
    $lastError = $_.Exception.Message
    try { Copy-Item -LiteralPath $Backup -Destination $Exe -Force -ErrorAction Stop } catch {
      $lastError = "Restore failed: $($_.Exception.Message)"
    }
    Start-Sleep -Milliseconds 500
  }
}
try {
  Copy-Item -LiteralPath $Backup -Destination $Exe -Force -ErrorAction Stop
  Set-Content -LiteralPath $Status -Value "Update failed; previous version restored: $lastError" -Encoding UTF8
} catch {
  Set-Content -LiteralPath $Status -Value "Update failed; restore old EXE from $Backup manually: $($_.Exception.Message)" -Encoding UTF8
}
exit 1
"#;
#[cfg(windows)]
fn install_after_exit(exe: &Path, staged: &Path, stage: &Path, working_dir: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let script = stage.join("install.ps1");
    fs::write(&script, INSTALL_SCRIPT)?;
    let backup: PathBuf = stage.join("music-terminal-player.previous.exe");
    let status = working_dir.join(DATA_DIR).join("update-status.txt");
    let window_mode = if std::env::args().any(|arg| arg == "--borderless") {
        "borderless"
    } else {
        "normal"
    };
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-ParentId")
        .arg(std::process::id().to_string())
        .arg("-Exe")
        .arg(exe)
        .arg("-Staged")
        .arg(staged)
        .arg("-Backup")
        .arg(backup)
        .arg("-WorkDir")
        .arg(working_dir)
        .arg("-Status")
        .arg(status)
        .arg("-WindowMode")
        .arg(window_mode)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start Windows update installer")?;
    Ok(())
}

#[cfg(not(windows))]
fn install_after_exit(_: &Path, _: &Path, _: &Path, _: &Path) -> Result<()> {
    bail!("Windows updater only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_version_comparison() {
        assert!(newer_release("v3.0.10", "3.0.9"));
        assert!(!newer_release("v3.0.9", "3.0.9"));
        assert!(!newer_release("v3.0.8", "3.0.9"));
        assert!(!newer_release("invalid", "3.0.9"));
    }

    #[test]
    fn windows_zip_selection() {
        let release = serde_json::json!({"assets": [
            {"name":"src.zip", "browser_download_url":"https://example.org/src.zip"},
            {"name":"music-terminal-player-win64.zip", "browser_download_url":"https://example.org/win.zip", "digest":"sha256:123"}
        ]});
        assert_eq!(
            select_zip_asset(&release).unwrap().0,
            "music-terminal-player-win64.zip"
        );
        let other_platform = serde_json::json!({"assets": [
            {"name":"music-terminal-player-linux.zip", "browser_download_url":"https://example.org/linux"},
            {"name":"music-terminal-player-windows-arm64.zip", "browser_download_url":"https://example.org/arm"}
        ]});
        assert!(select_zip_asset(&other_platform).is_none());
        let generic_zip = serde_json::json!({"assets": [
            {"name":"music-terminal-player-v3.1.0.zip", "browser_download_url":"https://example.org/release.zip"},
            {"name":"music-terminal-player-linux.zip", "browser_download_url":"https://example.org/linux.zip"}
        ]});
        assert_eq!(
            select_zip_asset(&generic_zip).unwrap().0,
            "music-terminal-player-v3.1.0.zip"
        );
    }

    #[test]
    fn stable_release_check_only_enables_manual_install_for_newer_windows_zip() {
        let release = serde_json::json!({
            "tag_name": "v3.0.10", "draft": false, "prerelease": false,
            "assets": [{
                "name":"music-terminal-player-windows-x64.zip",
                "browser_download_url":"https://github.com/harrytien107/Music_Terminal_Player/releases/download/v3.0.10/music-terminal-player-windows-x64.zip"
            }]
        });
        let available = classify_release(&release, "3.0.9").unwrap();
        assert!(available.can_install());
        assert!(matches!(available, UpdateState::Available(_, None)));
        assert!(!classify_release(&release, "3.0.10").unwrap().can_install());
        assert!(matches!(
            classify_release(&release, "3.0.10").unwrap(),
            UpdateState::UpToDate(_)
        ));

        let mut no_zip = release.clone();
        no_zip["assets"] = serde_json::json!([]);
        assert!(matches!(
            classify_release(&no_zip, "3.0.9").unwrap(),
            UpdateState::Available(_, None)
        ));
        assert!(!classify_release(&no_zip, "3.0.9").unwrap().can_install());

        let mut prerelease = release.clone();
        prerelease["prerelease"] = serde_json::json!(true);
        assert!(classify_release(&prerelease, "3.0.9").is_err());
    }

    #[test]
    fn manual_update_is_unavailable_without_a_compatible_download() {
        let state = UpdateState::Available(
            ReleaseInfo {
                tag: "v3.0.10".into(),
                asset: None,
            },
            None,
        );
        assert!(!state.can_install());
        assert!(!UpdateState::Checking.can_install());
        assert!(!UpdateState::Downloading("v3.0.10".into()).can_install());
        assert!(!UpdateState::Staged("v3.0.10".into()).can_install());
    }

    #[test]
    fn repeated_download_requests_cannot_start_a_second_installation() {
        let mut state = UpdateState::Available(
            ReleaseInfo {
                tag: "v3.0.10".into(),
                asset: Some(ZipAsset {
                    name: "release.zip".into(),
                    url: "https://example.org/release.zip".into(),
                    digest: None,
                }),
            },
            None,
        );
        assert!(take_installable_release(&mut state).is_some());
        assert!(matches!(state, UpdateState::Downloading(_)));
        assert!(take_installable_release(&mut state).is_none());
    }

    #[test]
    fn windows_exe_header_must_be_x64_pe() {
        let mut bytes = vec![0u8; 1024 * 1024];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&(128u32).to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        assert!(valid_windows_x64_executable(&bytes));
        bytes[132..134].copy_from_slice(&0x014cu16.to_le_bytes());
        assert!(!valid_windows_x64_executable(&bytes));
        assert!(!valid_windows_x64_executable(b"MZ"));
    }

    #[cfg(windows)]
    #[test]
    fn release_zip_extracts_expected_exe_and_rejects_wrong_file() {
        let dir = std::env::temp_dir().join(format!(
            "music-terminal-update-zip-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("music-terminal-player.exe");
        let mut data = vec![0u8; 1048577];
        data[..2].copy_from_slice(b"MZ");
        data[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        data[128..132].copy_from_slice(b"PE\0\0");
        data[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        fs::write(&exe, &data).unwrap();
        let compression_script = dir.join("compress.ps1");
        fs::write(
            &compression_script,
            "param([string]$File,[string]$Zip)\n$ErrorActionPreference = 'Stop'\nCompress-Archive -LiteralPath $File -DestinationPath $Zip -Force\n",
        )
        .unwrap();
        let make_zip = |file: &Path, zip: &Path| {
            let output = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-File"])
                .arg(&compression_script)
                .arg("-File")
                .arg(file)
                .arg("-Zip")
                .arg(zip)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let zip = dir.join("test.zip");
        make_zip(&exe, &zip);
        let extracted = dir.join("extracted.exe");
        extract_release_exe(&zip, &extracted, &dir).unwrap();
        assert_eq!(fs::read(&extracted).unwrap(), data);
        assert!(valid_windows_x64_executable(&fs::read(&extracted).unwrap()));
        fs::rename(&exe, dir.join("wrong-name.exe")).unwrap();
        let badzip = dir.join("bad.zip");
        make_zip(&dir.join("wrong-name.exe"), &badzip);
        assert!(extract_release_exe(&badzip, &extracted, &dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_installs_after_parent_exit_and_restores_backup_on_failure() {
        fn run_case(valid_update: bool) {
            let dir = std::env::temp_dir().join(format!(
                "music-terminal-installer-test-{}-{}",
                std::process::id(),
                valid_update
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let script = dir.join("install.ps1");
            fs::write(&script, INSTALL_SCRIPT).unwrap();
            let exe = dir.join("music-terminal-player.exe");
            let staged = dir.join("staged.exe");
            let backup = dir.join("previous.exe");
            let status = dir.join("status.txt");
            let previous = b"previous executable bytes";
            fs::write(&exe, previous).unwrap();
            if valid_update {
                fs::copy(
                    Path::new(&std::env::var("WINDIR").unwrap()).join("System32/whoami.exe"),
                    &staged,
                )
                .unwrap();
            } else {
                fs::write(&staged, b"invalid executable").unwrap();
            }
            let output = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script)
                .arg("-ParentId")
                .arg("2147483647") // Simulates an application that has already exited.
                .arg("-Exe")
                .arg(&exe)
                .arg("-Staged")
                .arg(&staged)
                .arg("-Backup")
                .arg(&backup)
                .arg("-WorkDir")
                .arg(&dir)
                .arg("-Status")
                .arg(&status)
                .arg("-WindowMode")
                .arg("normal")
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                valid_update,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read(&backup).unwrap(), previous);
            if valid_update {
                assert_eq!(fs::read(&exe).unwrap(), fs::read(&staged).unwrap());
                assert!(
                    fs::read_to_string(&status)
                        .unwrap()
                        .contains("installed successfully")
                );
            } else {
                assert_eq!(fs::read(&exe).unwrap(), previous);
                assert!(
                    fs::read_to_string(&status)
                        .unwrap()
                        .contains("previous version restored")
                );
            }
            let _ = fs::remove_dir_all(dir);
        }
        run_case(true);
        run_case(false);
    }
}
