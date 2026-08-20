# Music Terminal Player

Lightweight, keyboard-driven terminal music player for Windows. Play local audio, stream synchronized Telegram public or private channels, manage playlists and queues, or stream YouTube audio through yt-dlp and FFmpeg.

![Music Terminal Player Telegram playback](docs/media/example1.png)

## Why?

A small terminal player for local libraries and online sources without the overhead of a desktop music app. It keeps playback, queues, playlists, Telegram channels, and YouTube audio in one keyboard-driven interface. Inspired by [lowfi](https://github.com/talwat/lowfi).

## Features

- Local file and folder playback
- Telegram public- and private-channel synchronization, streaming, and offline download
- Combined multi-channel Telegram queues
- YouTube video and playlist audio streaming without saving complete media files
- YouTube seeking, 0.5x–2.0x speed control, and persistent per-category SponsorBlock auto-skip settings
- Windows media controls with track metadata and Play, Pause, Next, and Previous actions
- Automatic YouTube recovery when the default audio output changes
- Editable queues, playlists, shuffle, loop modes, and persistent volume
- Built-in offline English UI with optional, verified Vietnamese download and persistent selection
- Unicode-aware terminal panels and queue columns
- Portable or existing yt-dlp and FFmpeg setup with manual portable tool updates

## Installing
Windows is supported. macOS and Linux support are planned.

Windows binaries are available in the [releases](https://github.com/harrytien107/Music_Terminal_Player/releases/tag/v3.0.4)

## Usage
```music-terminal-player.exe``` 

The launcher provides local playback, Telegram login and channel actions, playlists, and YouTube audio. English is built into the executable and always works offline. Open `Language`, then select `Tiếng Việt — Download` to explicitly fetch the optional pack from the repository's raw `languages/` directory. Verified packs are cached under `.music-terminal/languages`, apply immediately, persist after restart, and continue working offline. Missing or invalid optional packs fall back to English. CLI commands are also available for local playback, Telegram synchronization, streaming, downloads, and YouTube playback.

See the [usage guide](docs/usage.md) for setup and command details.

## Controls

- `p` / `Space`: play or pause
- `n` / `v`: next or previous track
- `Up` / `Down` or `+` / `-`: volume up or down 10%
- `Right` / `Left`: volume up or down 1% for local/Telegram; seek forward/backward 10 seconds for YouTube
- `,` / `.`: decrease or increase YouTube speed by 0.25x (0.5x–2.0x)
- `s`: toggle all configured YouTube SponsorBlock categories for the current queue session
- `r`: toggle shuffle for unplayed tracks; manually queued tracks keep their order
- `l`: cycle loop mode
- `u`: open the queue
- `b` / `Esc`: return to the previous menu; launcher `Esc` keeps the player open
- `q` / `Ctrl+C`: quit completely; the launcher's `Quit` option also exits

Playback consumes each track-list entry once per pass. Duplicate entries and manually queued copies are separate plays; manual Previous may replay an entry. Loop Off stops after the remaining track list is exhausted, while Loop All starts a new full pass. The queue stays open across automatic track changes. `Delete` unqueues only a selected `Next in queue` item, leaving or returning one copy under `Next from track list`.

Windows System Media Transport Controls publish the current title, source, and play state. Play, Pause, Next, and Previous work from supported keyboards and Windows media surfaces. If an output device disconnects or Windows resumes after sleep or hibernation, YouTube waits for the current default device and automatically resumes near its previous position. Local and Telegram playback stop with a device-unavailable message; Telegram also cancels the active download and clears its temporary cache.

## Supported extensions

`mp3`, `flac`, `wav`, `ogg`, `opus`, `m4a`, `aac`, `alac`, `aiff`, `webm`.

## Extra Flags

- `--borderless`: remove the classic Windows Console Host border and size the window for the player. Windows Terminal tabs are unchanged.
- `--help` / `-h`: print CLI usage and controls.

Example:

```
music-terminal-player.exe --borderless
```

## Documentation

See the [usage guide](docs/usage.md) for Telegram setup, YouTube tools, CLI commands, queues, playlists, data files, and current limitations.