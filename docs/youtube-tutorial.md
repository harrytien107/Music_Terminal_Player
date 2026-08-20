# YouTube audio tutorial

Use this tutorial to play audio from a YouTube video, multiple videos, or a playlist.

## Before you start

You need an internet connection. The player also needs yt-dlp and FFmpeg. The easiest setup lets the player download portable copies beside the application.

## First-time setup

1. Open `music-terminal-player.exe`.
2. Select `Play YouTube audio`, then press `Enter`.
3. Select `Play YouTube URL or playlist`.
4. Select `Download portable yt-dlp and FFmpeg`.
5. Wait for both downloads and FFmpeg extraction to finish.

The tools are stored in a `tools` folder beside the executable. Their saved locations are stored in `.music-terminal/youtube-tools.txt`.

If yt-dlp and FFmpeg are already installed, select `Use existing yt-dlp and FFmpeg installations` instead. Enter a full executable path or a command available on `PATH`.

## Play YouTube audio

1. Open `Play YouTube audio`.
2. Select `Play YouTube URL or playlist`.
3. Paste one of these:
   - One video URL.
   - Multiple video URLs separated by spaces.
   - One playlist URL.
4. Press `Enter` and wait while metadata and the stream are resolved.

Example input:

```text
https://www.youtube.com/watch?v=VIDEO_ID
```

The player streams audio through FFmpeg. It does not save a complete YouTube media file.

## Add another video while playing

1. Press `a` during playback or while the queue is open.
2. Paste another video or playlist URL.
3. Press `Enter`.

The new tracks are added under `Next in queue` while current audio continues.

## Useful playback keys

| Key | Action |
| --- | --- |
| `p` or `Space` | Play or pause |
| `n` | Next track |
| `v` | Previous track |
| `Left` / `Right` | Seek backward or forward 10 seconds |
| `,` / `.` | Change speed by 0.25x |
| `s` | Toggle configured SponsorBlock skipping for this queue |
| `a` | Add another URL or playlist |
| `u` | Open the queue |
| `b` or `Esc` | Return to the previous menu |
| `q` or `Ctrl+C` | Quit completely |

## Configure SponsorBlock

1. Open `Play YouTube audio`.
2. Select `SponsorBlock categories`.
3. Toggle the categories you want automatically skipped.
4. Select `Back` when finished.

Settings persist in `.music-terminal/youtube-sponsorblock.txt`. Playback continues normally when no SponsorBlock timestamp exists.

## Check or update tools

1. Open `Play YouTube audio`.
2. Select `YouTube tools and updater`.
3. Select `Check for tool updates`.
4. If an update is available, select the yt-dlp, FFmpeg, or combined update action.

The built-in updater changes only portable tools in the application-relative `tools` folder. It does not overwrite external or `PATH` installations.

## If playback fails

- Confirm the URL opens in a browser.
- Open the updater and check for a newer yt-dlp version.
- Confirm FFmpeg is still present at the configured path.
- Check the internet connection.
- Retry later if YouTube temporarily rejects the stream.

For every option and command, see the [full usage guide](usage.md).
