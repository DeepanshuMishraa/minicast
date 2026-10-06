# minicast

A small macOS CLI for livestreaming your screen, camera and mic to any RTMP server (YouTube, Twitch, ...). One tuned ffmpeg process does the work, so CPU and memory stay low for long streams.

- Screen capture with the camera as a picture-in-picture overlay
- Hardware H.264 encoding (VideoToolbox), x264 as a fallback
- Lossless mic capture through a tiny Swift helper (ffmpeg's own mic input drops about 11% of audio buffers)
- Runs in the background and reconnects with backoff after network drops
- Logs are capped, so a stream can run for days without filling your disk

## Requirements

- macOS 13 or later
- [ffmpeg](https://ffmpeg.org) with avfoundation and VideoToolbox: `brew install ffmpeg`
- Xcode command line tools, for `swiftc`: `xcode-select --install`
- [Rust](https://rustup.rs), to build
- Screen recording, camera and microphone permission for your terminal app (System Settings > Privacy & Security)

## Install

```sh
git clone https://github.com/DeepanshuMishraa/minicast.git
cd minicast
cargo install --path .
```

## Quick start

```sh
minicast init                      # write ~/.config/minicast/config.toml
minicast devices                   # list screens, cameras and mics
minicast set \
  --screen "Capture screen 0" \
  --camera-device "MacBook Air Camera" \
  --mic "MacBook Air Microphone" \
  --bitrate 6000 \
  --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-STREAM-KEY
minicast validate                  # check the config
minicast start --detach --name yt  # go live
```

On the first run minicast compiles the mic helper once and caches it in `~/.config/minicast/bin`.

## Commands

| Command | What it does |
| --- | --- |
| `minicast init` | Create the config file (`--force` resets it) |
| `minicast devices` | List screens, cameras and microphones |
| `minicast set [flags]` | Change settings; with no flags, print the current ones |
| `minicast config` | Show the config file |
| `minicast validate` | Check the config without streaming |
| `minicast start` | Go live in the foreground (Ctrl-C stops) |
| `minicast start --detach --name yt` | Go live in the background |
| `minicast start --dry-run` | Print the ffmpeg command and exit |
| `minicast stream list` | Show running streams |
| `minicast stream logs yt` | Tail a stream's log |
| `minicast stream stop yt` / `--all` | Stop one or all streams |
| `minicast help [topic]` | Longer guide: `quickstart`, `youtube`, `audio`, `quality`, `retry`, `troubleshoot` |

Common `set` flags:

```sh
minicast set --resolution 1920x1080 --fps 60 --bitrate 6000
minicast set --camera true --camera-pos top-right --camera-size 320x180
minicast set --camera-filter studio --mic-mode voice
minicast set --mic off --system-audio "BlackHole 2ch"
minicast set --encoder x264          # software encode with true CBR
minicast set --auto-reconnect true --max-retries infinite
```

Run `minicast set --help` for every flag.

## Notes

- **Bitrate.** Pin one with `--bitrate`. 6000 kbps suits 1080p60. `--bitrate auto` measures your upload speed, but the probe is noisy, so it is off by default.
- **Low bitrate warning on YouTube.** VideoToolbox encodes a mostly static screen well below the target bitrate. The warning is harmless. `--encoder x264` pads to a constant bitrate if you want it gone, at a higher CPU cost.
- **Frame rate.** 60 is the most RTMP platforms accept, even on a 100 Hz or 120 Hz display.
- **Config and logs** live in `~/.config/minicast/`. Config edits never affect a running stream; each start takes a snapshot. The config file holds your stream key, so keep it private.
- **Stop streams with** `minicast stream stop`, not by killing ffmpeg. The supervisor would restart it.

## License

[MIT](LICENSE)
