use crate::util::load_setting_value;

pub(crate) const SETTING_KEY: &str = "app.startup_window_size";
pub(crate) const RELAUNCH_MARKER: &str = "--mtp-window-child";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupWindowSize {
    Default,
    Small,
    Medium,
    Large,
    Custom(u16, u16),
}

impl StartupWindowSize {
    pub(crate) fn load() -> Self {
        load_setting_value(SETTING_KEY)
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or(Self::Default)
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            _ => {
                let custom = value.strip_prefix("custom:")?;
                let (columns, rows) = parse_dimensions(custom)?;
                Some(Self::Custom(columns, rows))
            }
        }
    }

    pub(crate) fn as_setting(self) -> String {
        match self {
            Self::Default => "default".to_string(),
            Self::Small => "small".to_string(),
            Self::Medium => "medium".to_string(),
            Self::Large => "large".to_string(),
            Self::Custom(columns, rows) => format!("custom:{columns},{rows}"),
        }
    }

    pub(crate) fn dimensions(self) -> Option<(u16, u16)> {
        match self {
            Self::Default => None,
            Self::Small => Some((52, 24)),
            Self::Medium => Some((80, 30)),
            Self::Large => Some((120, 36)),
            Self::Custom(columns, rows) => Some((columns, rows)),
        }
    }
}

pub(crate) fn parse_dimensions(value: &str) -> Option<(u16, u16)> {
    let (columns, rows) = value.trim().split_once([',', 'x', 'X', '×'])?;
    let columns = columns.trim().parse::<u16>().ok()?;
    let rows = rows.trim().parse::<u16>().ok()?;
    (columns >= 40 && rows >= 20).then_some((columns, rows))
}

pub(crate) fn consume_relaunch_marker(args: &mut Vec<String>) -> bool {
    if args.first().is_some_and(|arg| arg == RELAUNCH_MARKER) {
        args.remove(0);
        true
    } else {
        false
    }
}

#[cfg(windows)]
pub(crate) fn terminal_command(
    exe: &std::path::Path,
    cwd: &std::path::Path,
    args: &[String],
    columns: u16,
    rows: u16,
) -> std::process::Command {
    let mut command = std::process::Command::new("wt.exe");
    command
        .arg("-w")
        .arg("new")
        .arg("--size")
        .arg(format!("{columns},{rows}"))
        .arg("new-tab")
        .arg("--startingDirectory")
        .arg(cwd)
        .arg(exe)
        .arg(RELAUNCH_MARKER)
        .args(args)
        .current_dir(cwd);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_sizes_round_trip_and_fallback_for_invalid_values() {
        for size in [
            StartupWindowSize::Default,
            StartupWindowSize::Small,
            StartupWindowSize::Medium,
            StartupWindowSize::Large,
            StartupWindowSize::Custom(101, 37),
        ] {
            assert_eq!(StartupWindowSize::parse(&size.as_setting()), Some(size));
        }
        assert_eq!(StartupWindowSize::Default.dimensions(), None);
        assert_eq!(StartupWindowSize::Small.dimensions(), Some((52, 24)));
        assert_eq!(StartupWindowSize::Medium.dimensions(), Some((80, 30)));
        assert_eq!(StartupWindowSize::Large.dimensions(), Some((120, 36)));
        for bad in [
            "",
            "random",
            "custom:39,20",
            "custom:40,19",
            "custom:1,1",
            "custom:xx,20",
            "custom:70000,20",
        ] {
            assert_eq!(StartupWindowSize::parse(bad), None);
        }
        assert_eq!(parse_dimensions(" 40x20 "), Some((40, 20)));
        assert_eq!(parse_dimensions(" 80 × 30 "), Some((80, 30)));
        assert_eq!(parse_dimensions("120,36"), Some((120, 36)));
    }

    #[test]
    fn relaunch_marker_is_consumed_once_and_preserves_user_arguments() {
        let mut args = vec![
            RELAUNCH_MARKER.to_string(),
            "play".into(),
            "D:\\Music\\a song.mp3".into(),
            "--borderless".into(),
        ];
        assert!(consume_relaunch_marker(&mut args));
        assert_eq!(args, ["play", "D:\\Music\\a song.mp3", "--borderless"]);
        assert!(!consume_relaunch_marker(&mut args));
    }

    #[cfg(windows)]
    #[test]
    fn wt_command_uses_new_window_size_and_preserves_paths_and_args() {
        let exe = std::path::Path::new(r"D:\Music\Player Test\music-terminal-player.exe");
        let cwd = std::path::Path::new(r"D:\Music\Work Space");
        let args = vec!["play".to_string(), "My Song.flac".to_string()];
        let command = terminal_command(exe, cwd, &args, 80, 30);
        assert_eq!(command.get_program(), "wt.exe");
        assert_eq!(command.get_current_dir(), Some(cwd));
        let passed: Vec<_> = command.get_args().collect();
        assert_eq!(
            passed,
            [
                "-w",
                "new",
                "--size",
                "80,30",
                "new-tab",
                "--startingDirectory",
                cwd.to_str().unwrap(),
                exe.to_str().unwrap(),
                RELAUNCH_MARKER,
                "play",
                "My Song.flac"
            ]
        );
    }
}
