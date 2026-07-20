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

Running or double-clicking the executable opens an arrow-key menu. Use `Up` / `Down` to select, `Enter` to confirm, and `Esc` to return. The first-row `Play shuffle (Telegram)` shortcut asks for a saved channel and immediately shuffles its synchronized song list. The menu displays whether a saved Telegram login exists and provides local folders and Telegram channels. The terminal is cleared whenever an action finishes or returns, so only the current menu remains visible. Command-line usage remains available through `--help`.

Saved channels and folder locations are stored in `.music-terminal-library` and ignored by Git. Forgetting a folder location only removes it from this list; it never deletes the folder or music files.

## Usage

```powershell
cargo run -- play .\music
cargo run -- play .\music\song.mp3
```

From the executable menu, choose local playback, then select `Add folder` once. Later runs only require selecting its saved location. Select `Forget location` to remove it from the menu without deleting the folder.

Controls:

- `p`: play/pause
- `Left` / `Right`: seek 10 seconds
- `n`: next track
- `v`: previous track
- `r`: toggle shuffle
- `l`: cycle loop mode through `off`, `all`, and `one`
- `+` / `-`: volume
- `b`: return to the executable menu
- `q`: quit

## Telegram login

Create an `api_id` and `api_hash` at https://my.telegram.org, then run:

```powershell
cargo run -- login
```

First setup asks for the API credentials, phone number, login code, and optional two-step password. The password is hidden while typing. Later runs reuse `.telegram.credentials` and `.telegram.session*`; both are ignored by Git.

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

Select `Update Telegram song list` once to create `.telegram-<channel>.tracks.txt`, a lightweight list containing every audio message ID and song name. This scan does not download music. Streaming reads the list instead of rescanning Telegram history and resolves only the current track from Telegram, so playback does not wait for every catalog entry. After selecting a channel, choose `Play in order`, `Shuffle`, or `Search and choose a track`. In search, type any part of a song name to update recommendations immediately, use Backspace to edit, `Up` / `Down` to select, `Enter` to play, and `Esc` to return. Select `Add channel` once; later actions only require selecting it. Select `Forget channel` to remove it.

Streaming downloads sequential 512 KiB Telegram chunks into `.telegram-cache`, starts after approximately 1 MiB is available, and continues while the track plays. The terminal displays `buffering` when playback catches the download and shows cache progress.

Seeking inside downloaded data is immediate. Seeking beyond downloaded data waits for the sequential download to reach that position.

When playback moves to another track, the previous track's cache file is deleted. Loop `all` restarts the list after its final song; loop `one` repeats the current song. Loop `off` returns to the menu after the final song. Pressing `b` returns to the menu after cleaning the cache. Pressing `q` or `Esc` quits after cleaning it. On Windows, closing the terminal with its `X` button also causes the OS to delete the active cache file; any stale cache is removed when Telegram streaming next starts.

## Playlists

Select `Playlists` from the launcher to play, create, edit, rename, or delete a persistent playlist. Choose `Telegram channel` to select songs from a synchronized Telegram catalog, or `Local folder` to select local audio files recursively. Type to filter songs, use `Up` / `Down` to move, press `Space` to toggle one or multiple songs, then press `Enter` to save. `Esc` cancels the editor. Missing local files are skipped during playback. Playlists are stored in `.music-terminal-playlists` and ignored by Git.

A playlist currently uses one source: one Telegram channel or local files. Changing its source replaces its selected songs. Mixed local and Telegram playlists are intentionally omitted because their playback engines use different queue types.

## Update a channel song list

```powershell
cargo run -- sync public_channel_username
```

Sync scans the complete channel, reports completion, total audio-song count, scan time, and saves the complete ordered song list to `.telegram-<channel>.tracks.txt`. It does not download audio. The executable keeps this result visible until Enter is pressed. Streaming requires this list, so update it before streaming and rerun sync when the channel changes.

## Download a channel for offline playback

```powershell
cargo run -- download public_channel_username .\music
```

Select `Download Telegram channel to .\music` to scan and update the song list, then download missing audio files to `.\music`. It displays current/total download progress and elapsed download time. Play completed files with `cargo run -- play .\music`.

## Supported audio extensions

`mp3`, `flac`, `wav`, `ogg`, `opus`, `m4a`, `aac`, `alac`, `aiff`, `webm`.

## Terminal UI

Local and Telegram playback use a bordered panel with current-track details, playback state, loop mode, cache state where applicable, and a moving progress bar. Menus, search, and players redraw in place rather than clearing the full terminal on each key press or timer update. No full TUI framework is used; Crossterm handles keyboard events and redraws.

## Deliberate limits

- Streaming uses sequential Telegram chunks rather than random byte-range requests. Add range retrieval when distant seeking needs to be immediate.
- Only the active streamed track is buffered; the previous track is deleted on change. Add persistent caching when offline replay is needed.
- Shuffle changes track order in memory and is reset when leaving the player.
