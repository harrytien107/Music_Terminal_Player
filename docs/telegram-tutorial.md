# Telegram music tutorial

Use this tutorial to log in, save a channel catalog, and stream its audio messages.

## What Telegram setup does

The player logs in to your Telegram account, scans audio messages from channels you can access, and saves small song catalogs under `.music-terminal/catalogs`. Synchronization does not download all music. Audio downloads progressively only when you play a track.

## Step 1: Create Telegram API credentials

1. Open <https://my.telegram.org> in a browser.
2. Sign in with your Telegram phone number.
3. Open `API development tools`.
4. Create an application if you do not already have one.
5. Copy the numeric `api_id` and the 32-character `api_hash`.

Keep the `api_hash` private.

## Step 2: Log in through the player

1. Open `music-terminal-player.exe`.
2. Select `Login to Telegram`, then press `Enter`.
3. Enter the `api_id`.
4. Enter the `api_hash`.
5. Enter your phone number in international format, including `+` and the country code. Example:

   ```text
   +84901234567
   ```

6. Enter the login code sent by Telegram.
7. Enter your two-step verification password if Telegram asks for it.

The player stores the login under `.music-terminal/telegram.credentials` and `.music-terminal/telegram.session*` so later launches can reuse it.

> Telegram credentials and sessions are not encrypted. Do not upload, commit, or share the `.music-terminal` folder.

## Step 3: Add and synchronize a channel

1. From the launcher, select `Sync and update Telegram channel`.
2. Choose the matching channel type:
   - `Add public channel by username` for a public username or `t.me` URL.
   - `Add private channel from this account` for a private broadcast channel already visible to your account.
   - `Add joined private channel by invite link` for a private channel you have already joined.
3. Select or enter the channel.
4. Wait for the scan to finish.

Synchronization saves song names and Telegram message IDs. Run the same action later to refresh the catalog with newer messages.

## Step 4: Stream synchronized music

1. From the launcher, select `Stream Telegram channel`.
2. Highlight a saved channel.
3. Press `Enter` to use that channel, or press `Space` to select multiple channels and then `Enter`.
4. Choose a playback option:
   - `Play in order`.
   - `Shuffle`.
   - `Search and choose a track`.
   - `Search and choose multiple tracks`.

Playback begins after enough of the current track has downloaded into the temporary cache.

## Search and selection keys

### Choose channels

| Key | Action |
| --- | --- |
| `Space` | Select or deselect one channel |
| `Ctrl+A` | Select or deselect all channels |
| `Enter` | Continue with selected channels, or the highlighted channel |
| `Esc` | Return |

### Search for multiple tracks

| Key | Action |
| --- | --- |
| Type | Filter songs by name |
| `Ctrl+Space` | Select or deselect one result |
| `Ctrl+A` | Select or deselect all matching results |
| `Enter` | Play selected tracks |
| `Esc` | Return |

The first selected song starts immediately. Other selected songs appear under `Next in queue`. Unselected channel songs remain under `Next from track list`.

## Useful playback keys

| Key | Action |
| --- | --- |
| `p` or `Space` | Play or pause |
| `n` | Next song |
| `v` | Previous song |
| `r` | Toggle shuffle |
| `l` | Change loop mode |
| `u` | Open the queue |
| `Up` / `Down` | Change volume by 10% |
| `Left` / `Right` | Change volume by 1% |
| `b` or `Esc` | Return to the previous menu |
| `q` or `Ctrl+C` | Quit completely |

## Download a channel for offline local playback

1. Synchronize the channel first.
2. From the launcher, select `Download Telegram channel to .\music`.
3. Select the saved channel.
4. Wait for the downloads to finish.
5. Play the downloaded files through `Play local folder`.

Existing files are skipped.

## If Telegram does not work

- Confirm the `api_id` and `api_hash` came from <https://my.telegram.org>.
- Include `+` and the country code in the phone number.
- Confirm your account can already view the private channel.
- Synchronize the channel before trying to stream it.
- If you changed Telegram accounts, forget and add private channels again.
- If one audio upload is corrupt or unsupported, press `n` to continue to another track.

For every option and command, see the [full usage guide](usage.md).
