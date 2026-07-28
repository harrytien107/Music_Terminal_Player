# Usage guide

Music Terminal Player currently supports Windows.

## Build and run

Build the release executable with Rust:

```powershell
cargo build --release
```

Run it:

```powershell
.\target\release\music-terminal-player.exe
```

Running or double-clicking the executable opens the launcher. `Play YouTube audio` is the second row for quick access. Use `Up` / `Down` to select, `Enter` to confirm, and `Esc` to return. Add `--borderless` to remove the classic Windows Console Host border and size its window to 1200×500 pixels:

```powershell
.\target\release\music-terminal-player.exe --borderless
```

Windows Terminal owns its shared top-level window, so `--borderless` does not alter Windows Terminal tabs. Command-line usage is available through `--help`.

## Player controls

- `p` / `Space`: play or pause
- `Up` / `Down`: volume up or down 10%
- `+` / `-`: volume up or down 10%
- `Right` / `Left`: volume up or down 1%
- `n`: next track
- `v`: previous track
- `r`: toggle shuffle
- `l`: cycle loop mode through `off`, `all`, and `one`
- `u`: open the editable playback queue
- `b`: return to the launcher
- `q` / `Ctrl+C`: quit

The queue has `Now playing`, `Next in queue`, and `Next from track list` columns. Use `Up` / `Down` to select, `Enter` to play, `Delete` to remove, `a` to add next, and `Esc` to close. Local and Telegram queues use existing-track search. YouTube prompts for another URL or playlist. Audio continues while the queue is open.

## Local playback

Play a folder or file from the CLI:

```powershell
cargo run -- play .\music
cargo run -- play .\music\song.mp3
```

From the launcher, add a local folder once and select its saved location on later runs. `Forget location` removes only the saved reference; it never deletes the folder or music files.

Supported extensions: `mp3`, `flac`, `wav`, `ogg`, `opus`, `m4a`, `aac`, `alac`, `aiff`, and `webm`.

## Telegram setup

Create an `api_id` and `api_hash` at <https://my.telegram.org>, then run:

```powershell
cargo run -- login
```

First setup asks for API credentials, phone number, login code, and an optional two-step password. Later runs reuse `.music-terminal/telegram.credentials` and `.music-terminal/telegram.session*`.

Credentials are not encrypted. Do not commit or share `.music-terminal`. Environment variables can supply credentials without saving them through the player:

```powershell
$env:TG_ID="123456"
$env:TG_HASH="your_api_hash"
```

Environment variables override saved API credentials.

## Telegram synchronization and streaming

Use `Sync and update Telegram channel` in the launcher to add or update either source:

- `Add public channel by username` accepts an existing public username or `t.me` URL.
- `Add private channel from this account` shows live dialog and private-channel counts while scanning the logged-in account, then lists every accessible private broadcast channel.
- `Add joined private channel by invite link` accepts `t.me/+...` and `t.me/joinchat/...` links only when the account is already a member. It never joins or requests access automatically. Submit an empty value or type `back` to return to the synchronization menu.

The synchronization menu groups saved channels and add/manage actions inside aligned rectangular section headers. `Sync and update All channels` synchronizes every saved channel sequentially. In the private-channel picker, type to search by title or numeric channel ID. The existing checkbox displays `[x]` for channels already added to the library or selected for the current synchronization. Already-saved channels remain visible but are locked: `Ctrl+Space`, `Ctrl+A`, and `Enter` cannot select them again. Use `Up` / `Down` or `Page Up` / `Page Down` to scroll, `Ctrl+Space` to toggle one unsaved channel, `Ctrl+A` to toggle all unsaved current matches, `Enter` to synchronize the selected channels, and `Esc` to cancel. If no channels are toggled, `Enter` synchronizes the highlighted unsaved channel. Multiple selected channels synchronize sequentially without clearing earlier scan output, each successful new channel is saved independently, and the final batch summary reports every channel as successful or failed.

`Forget channel` supports one or many saved channels. Use `Space` to toggle one, `Ctrl+A` to toggle all, arrows or Page Up/Page Down to scroll, `Enter` to forget, and `Esc` to cancel. With no toggled channels, `Enter` forgets only the highlighted channel.

The scan stores audio message IDs and song names under `.music-terminal/catalogs` without downloading music. An empty scan reports Telegram's message and document counts and does not overwrite an existing catalog. Saved private-channel access is tied to the Telegram session; after changing accounts or deleting the session, forget and add that private channel again.

CLI synchronization:

```powershell
cargo run -- sync public_channel_username
```

From `Stream Telegram channel`, choose one or multiple saved channels. Use `Space` to toggle one, `Ctrl+A` to toggle all, arrows or Page Up/Page Down to scroll, `Enter` to play, and `Esc` to return. With no toggled channels, `Enter` plays only the highlighted channel. Multiple selected catalogs are combined into one playback queue while each track keeps its original channel identity. This menu alone uses plain `Space`; searchable selection screens continue to use `Ctrl+Space` so spaces can be typed into search text.

Stream a synchronized public channel from the CLI:

```powershell
cargo run -- stream public_channel_username
cargo run -- stream "@public_channel_username"
```

CLI Telegram commands continue to accept public usernames. Private channels are selected and saved through the launcher because they do not have public usernames.

Streaming resolves only the current track and downloads sequential 512 KiB chunks into `.music-terminal/telegram-cache`. Playback starts after roughly 1 MiB is available. Temporary network errors retry from the next chunk. The player displays buffering, reconnecting, and cache progress states.

The previous track's cache file is deleted on track changes. Returning to the launcher or quitting cleans active cache files. Stale cache is removed when Telegram streaming starts again.

`All synchronized channels` combines saved catalogs into one queue while retaining channel identity internally for playback. Player and queue labels show song names without channel prefixes.

## Telegram offline download

Download a synchronized channel into a local folder:

```powershell
cargo run -- download public_channel_username .\music
```

The download action updates the catalog, skips existing files, and reports progress. Play the result with local playback.

## YouTube audio

On first use, choose one setup option:

- `Download portable yt-dlp and FFmpeg` downloads both Windows executables into a `tools` folder beside the player executable and automatically saves both executable locations in `.music-terminal/youtube-tools.txt`. Portable setup uses Windows `curl.exe` and `tar.exe` and requires access to GitHub and gyan.dev.
- `Use existing yt-dlp and FFmpeg installations` accepts full executable paths or commands available on `PATH`.

Tool paths are saved in `.music-terminal/youtube-tools.txt`. yt-dlp is validated with `--version`; FFmpeg is validated with `-version`.

Start YouTube playback from the launcher or CLI:

```powershell
cargo run -- youtube
```

Paste one video URL, multiple space-separated URLs, or a playlist URL. yt-dlp expands metadata and resolves a fresh audio URL before each track. FFmpeg pipes decoded 48 kHz stereo PCM directly to the player. No complete YouTube media file or YouTube cache is created.

Press `a` during playback or in the YouTube queue to add another URL or playlist to `Next in queue`. Current audio continues while metadata resolves.

## Playlists

The launcher can create, play, edit, rename, and delete persistent playlists. Telegram playlists may combine tracks from synchronized public and private channels. Local and Telegram tracks remain separate playlist sources because they use different playback engines.

Search updates while typing. Use `Backspace` to edit, `Up` / `Down` to select, `Ctrl+Space` to toggle a track, `Ctrl+A` to toggle all matches, `Enter` to save, and `Esc` to cancel. Plain `Space` remains available in search text. Missing local files are skipped during playback.

Playlists are stored in `.music-terminal/playlists.txt`.

## Data files

Player data lives under `.music-terminal`, which is ignored by Git. It contains settings, saved libraries, playlists, Telegram credentials and sessions, channel catalogs, YouTube tool paths, and temporary Telegram stream cache. Existing root-level data files migrate automatically.

## Terminal UI notes

Long and wide Unicode song names are clipped to panel and queue widths. Menus, search screens, queues, and players redraw in place with Crossterm. No full TUI framework is used.

Current limits:

- Windows is supported; macOS and Linux support are planned.
- Telegram streaming uses sequential chunks rather than random byte-range requests.
- Only the active Telegram track is buffered.
- Shuffle order is in memory and resets when leaving the player.
