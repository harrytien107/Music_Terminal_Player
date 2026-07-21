# Music Terminal Player

Lightweight terminal music player with local playback, Telegram public-channel streaming, and offline sync. Inspired by lowfi.

![Music Terminal Player Telegram playback](docs/media/example1.png)

## Why?

A small, keyboard-driven player for local music and Telegram channels without the overhead of a desktop music app. The interface and lightweight approach are inspired by lowfi.

## Installing

Windows is currently supported. macOS and Linux support are planned.

Build the release executable with Rust:

```powershell
cargo build --release
```

Run:

```powershell
.\target\release\music-terminal-player.exe
```

Running or double-clicking the executable opens an arrow-key menu with grouped action icons and Telegram login status. Add `--borderless` to remove the classic Windows Console Host border and size its window to 1200×500 pixels: `music-terminal-player.exe --borderless`. Windows Terminal owns its shared top-level window, so the flag leaves Windows Terminal tabs unchanged. Use `Up` / `Down` to select, `Enter` to confirm, and `Esc` to return. The first-row shuffle shortcut asks for a saved channel and immediately shuffles its synchronized song list. The menu provides local folders and Telegram channels. The terminal is cleared whenever an action finishes or returns, so only the current menu remains visible. Command-line usage remains available through `--help`.

Settings and player data are organized under the `.music-terminal` folder and ignored by Git. It contains `settings.txt`, `library.txt`, `playlists.txt`, Telegram credentials/session data, channel catalogs, and temporary stream cache. Existing root-level data files migrate there automatically. The player does not change the folder's filesystem attributes. Forgetting a folder location only removes it from the saved library; it never deletes the folder or music files.

## Usage

```powershell
cargo run -- play .\music
cargo run -- play .\music\song.mp3
```

From the executable menu, choose local playback, then select `Add folder` once. Later runs only require selecting its saved location. Select `Forget location` to remove it from the menu without deleting the folder.

Controls:

- `p` / `Space`: play/pause
- `Up` / `Down`: volume up/down 10%
- `+` / `-`: volume up/down 10%
- `Right` / `Left`: volume up/down 1%
- `n`: next track
- `v`: previous track
- `r`: toggle shuffle
- `l`: cycle loop mode through `off`, `all`, and `one`
- `u`: open the editable playback queue
- `b`: return to the executable menu
- `q` / `Ctrl+C`: quit

The on-screen table shows only the primary key for actions with aliases.

## Telegram login

Create an `api_id` and `api_hash` at https://my.telegram.org, then run:

```powershell
cargo run -- login
```

First setup asks for the API credentials, phone number, login code, and optional two-step password. Invalid interactive input stays in the login flow so it can be entered again; launcher login failures offer retry or return to the menu. The password is hidden while typing. Later runs reuse `.music-terminal/telegram.credentials` and `.music-terminal/telegram.session*`; both are ignored by Git.

The credentials are hidden from the normal project view but are not encrypted. Do not commit or share `.music-terminal`. Embedding `api_id` and `api_hash` in the executable would not secure them because executable contents can be inspected. Use the `TG_ID` and `TG_HASH` environment variables when credentials must not be stored by the player.

Environment variables override the saved API credentials:

```powershell
$env:TG_ID="123456"
$env:TG_HASH="your_api_hash"
```

## Stream from a public Telegram channel

```powershell
cargo run -- stream public_channel_username
cargo run -- stream "@public_channel_username"
```

Select `Update Telegram song list` once to create `.music-terminal/catalogs/<channel>.tracks.txt`, a lightweight list containing every audio message ID and song name. This scan does not download music. Streaming reads the list instead of rescanning Telegram history and resolves only the current track from Telegram, so playback does not wait for every catalog entry. Selecting a channel without a usable synchronized list displays the problem and returns to the previous menu instead of closing the program. After selecting a channel, choose `Play in order`, `Shuffle`, or `Search and choose a track`. In search, type any part of a song name to update recommendations immediately, use Backspace to edit, `Up` / `Down` to select, `Enter` to play, and `Esc` to return. Select `Add channel` once; later actions only require selecting it. Select `Forget channel` to remove it.

Streaming downloads sequential 512 KiB Telegram chunks into `.music-terminal/telegram-cache`, starts after approximately 1 MiB is available, and continues while the track plays. Temporary network and transport errors retry every two seconds from the next chunk instead of closing the player. The terminal displays `buffering` when playback catches the download, `reconnecting` during retry, and cache progress.

When playback moves to another track, the previous track's cache file is deleted. Loop `all` restarts the list after its final song; loop `one` repeats the current song. Loop `off` returns to the menu after the final song. Press `u` to view the current queue as `Now playing`, `Next in queue`, and `Next from track list` columns. Choose a future track, remove it with `Delete`, or press `a` to add selected songs to `Next in queue`. Added songs play in source-list order, then playback resumes `Next from track list`. Audio continues while the queue screen is open and advances when the current song finishes. Pressing `b` returns to the menu after cleaning the cache. Pressing `q`, `Ctrl+C`, or `Esc` quits after cleaning it. On Windows, closing the terminal with its `X` button also causes the OS to delete the active cache file; any stale cache is removed when Telegram streaming next starts.

## Playlists

Select `Playlists` from the launcher to play, create, edit, rename, or delete a persistent playlist. Choose `Telegram channel` to select songs from a synchronized Telegram catalog, or `Local folder` to select local audio files recursively. Type to filter songs, use `Up` / `Down` to move, press `Space` to toggle one song, or press `Ctrl+A` to select/deselect every current search match. Press `Enter` to save; `Esc` cancels. Missing local files are skipped during playback. Playlists are stored in `.music-terminal/playlists.txt` and ignored by Git.

A playlist currently uses one source: one Telegram channel or local files. Changing its source replaces its selected songs. Mixed local and Telegram playlists are intentionally omitted because their playback engines use different queue types.

## Update a channel song list

```powershell
cargo run -- sync public_channel_username
```

Sync scans the complete channel, reports completion, total audio-song count, scan time, and saves the complete ordered song list under `.music-terminal/catalogs`. It does not download audio. The executable keeps this result visible until Enter is pressed. Streaming requires this list, so update it before streaming and rerun sync when the channel changes.

## Download a channel for offline playback

```powershell
cargo run -- download public_channel_username .\music
```

Select `Download Telegram channel to .\music` to scan and update the song list, then download missing audio files to `.\music`. It displays current/total download progress and elapsed download time. Play completed files with `cargo run -- play .\music`.

## Supported audio extensions

`mp3`, `flac`, `wav`, `ogg`, `opus`, `m4a`, `aac`, `alac`, `aiff`, `webm`.

## Terminal UI

Local and Telegram playback use a bordered panel with current-track details, playback state, loop mode, cache state where applicable, a moving progress bar, and an aligned keybind table. The three-column queue manager supports `Up` / `Down`, `Enter`, `Delete`, and an `a` search/selection screen with `Space`, `Ctrl+A`, and `Enter` to play the selection next. Menus, search, and players redraw in place rather than clearing the full terminal on each key press or timer update. No full TUI framework is used; Crossterm handles keyboard events and redraws.

## Deliberate limits

- Streaming uses sequential Telegram chunks rather than random byte-range requests.
- Only the active streamed track is buffered; the previous track is deleted on change. Add persistent caching when offline replay is needed.
- Shuffle changes track order in memory and is reset when leaving the player.
