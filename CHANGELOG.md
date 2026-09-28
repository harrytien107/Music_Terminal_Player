# Changelog

## v3.0.7 - 2026-09-28

### Added

- Added a dedicated **Settings** menu.
- Added per-item main-menu visibility controls so individual launcher actions can be hidden while Settings always remains available.
- Added an option to show or hide Telegram public/private channel icons.
- Added an option to show or hide playback key-binding panels in Local, Telegram, and YouTube players.
- Added per-channel track counts in the Telegram channel picker.
- Added per-folder track counts in the Local folder picker.
- Added persistent manual ordering for saved Telegram channels and Local folders with `Shift+↑/↓`.

### Changed

- Moved the language selector into Settings.
- Updated launcher presentation: Quick Play is white and the Telegram launcher uses the music-note icon.
- Replaced emoji-style Settings/tool icons with monochrome terminal-friendly symbols where appropriate.
- Shortened navigation hints to use `↑/↓` instead of the longer `Up/Down/Page Up/Page Down` wording while keeping Page Up/Page Down behavior available where supported.
- Updated the Vietnamese language pack to version 9 and synchronized its navigation/help text with the new UI.

### Fixed

- Settings now keeps the cursor on the option that was toggled or opened instead of jumping back to the first row.
- YouTube URL entry can return with `Esc`; invalid URLs stay in the URL prompt and show the error instead of returning to the main menu.
- YouTube URL input now handles terminal paste events directly.
- Telegram selection state follows channels correctly when channels are reordered.
- Settings persistence now preserves existing values such as volume when boolean options are changed.

