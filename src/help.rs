//! Long-form user guide for `minicast help [topic]`.
//!
//! Clap still renders per-command `--help`; this module adds the narrative
//! layer on top: quickstart, device selection, YouTube setup, quality and
//! retry behaviour, troubleshooting.

pub const OVERVIEW: &str = "\
minicast — minimal macOS livestreaming CLI (screen + camera PiP + mic -> RTMP)\n\
\n\
QUICKSTART (YouTube, 60 seconds)\n\
  1. minicast init                          # create ~/.config/minicast/config.toml\n\
  2. minicast devices                       # note screen, camera and mic IDs\n\
  3. minicast set --screen 1 --camera-device 0 --mic 1 --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY\n\
  4. minicast validate                      # sanity-check everything\n\
  5. minicast start --detach --name yt       # go live (survives network drops)\n\
  6. minicast stream list                   # watch uptime; minicast stream logs yt\n\
\n\
COMMANDS\n\
  init        Create (or reset with --force) the config file.\n\
  devices     List screens, cameras and microphones with their numeric IDs.\n\
  config      Show the config file (show) or just its path (path).\n\
  set         Change settings. No flags = print current values.\n\
              Screen, camera AND mic all accept a numeric ID or a name.\n\
  validate    Check the config without streaming.\n\
  start       Go live. --dry-run prints the ffmpeg command; --detach runs\n\
              supervised in the background with auto-retry.\n\
  stream      list | stop <id|name>|--all | logs <id|name> for detached runs.\n\
  help        This guide. `minicast help <topic>` for a deep dive.\n\
\n\
TOPICS — `minicast help <topic>`\n\
  quickstart  first stream, step by step            audio      mics by ID, mic modes\n\
  youtube     stream-key setup, 1080p best settings  camera     PiP look presets\n\
  devices     picking screen/camera/mic IDs          quality    bitrate / fps / encoder\n\
  set         every --set flag explained             retry      auto-reconnect behaviour\n\
  start       foreground vs detached                 record     local backup recordings\n\
  troubleshoot  fixes for common failures (lag, crackle)\n\
\n\
Every command also has `minicast <command> --help` with copy-paste examples.\n\
Config lives at ~/.config/minicast/config.toml (override with -c/--config).\n";

pub const QUICKSTART: &str = "\
QUICKSTART — your first stream\n\
\n\
  minicast init\n\
  minicast devices\n\
    screens:        e.g. [1] Capture screen 0\n\
    cameras:        e.g. [0] FaceTime HD Camera\n\
    audio devices:  e.g. [1] MacBook Air Microphone\n\
\n\
  minicast set --screen 1 --camera-device 0 --mic 1 \\\n\
      --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY\n\
\n\
  minicast validate\n\
  minicast start --detach --name yt\n\
  minicast stream list\n\
\n\
Going live for real? Read `minicast help youtube` for the key + 1080p recipe,\n\
and `minicast help retry` to understand why a dead network won't end your stream.\n";

pub const YOUTUBE: &str = "\
YOUTUBE SETUP\n\
\n\
  1. YouTube Studio -> Create -> Go live -> Stream.\n\
  2. Copy the stream key (never share it — it grants broadcast access).\n\
  3. minicast set --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY\n\
  4. Recommended 1080p30 settings (YouTube's sweet spot):\n\
\n\
       minicast set --resolution 1920x1080 --fps 30 --bitrate 6000\n\
\n\
     720p on slow upload:  --resolution 1280x720  --fps 30 --bitrate 3500\n\
     1080p60 gaming:       --resolution 1920x1080 --fps 60 --bitrate 9000\n\
\n\
  5. minicast validate && minicast start --detach --name yt\n\
  6. Watch Studio's preview, then press Go Live there.\n\
\n\
 Twitch uses  rtmp://live.twitch.tv/app/KEY   (see `minicast help set`).\n\
 The key is stored chmod-600 and masked as **** in `stream list` output.\n";

pub const DEVICES_TOPIC: &str = "\
DEVICES — screen, camera and mic all work the same way: by ID or by name\n\
\n\
  minicast devices\n\
    screens:        [1] Capture screen 0\n\
    cameras:        [0] FaceTime HD Camera\n\
    audio devices:  [1] MacBook Air Microphone\n\
\n\
  Use the bracketed ID (stable for this boot, unambiguous):\n\
    minicast set --screen 1 --camera-device 0 --mic 1\n\
\n\
  …or a name/substring (matched case-insensitively):\n\
    minicast set --screen \"Capture screen 0\" --mic \"MacBook Air\"\n\
\n\
  Names are resolved to IDs at `set` time. Re-run `devices` after plugging\n\
  hardware in/out — macOS renumbers IDs when devices appear or disappear.\n\
  \"off\" disables an audio source:  minicast set --mic off\n";

pub const AUDIO: &str = "\
AUDIO — mic and system audio, both selectable by ID\n\
\n\
  minicast devices                       # audio devices section lists IDs\n\
  minicast set --mic 1                   # by ID (same as --screen/--camera-device)\n\
  minicast set --mic \"MacBook Air\"      # …or by name/substring\n\
  minicast set --mic off                 # mute the mic\n\
\n\
  Mic DSP presets (--mic-mode, like macOS mic modes):\n\
    standard   transparent — rumble/plosive filter + clip guard only (default)\n\
    voice      Voice-Isolation-like — FFT noise suppression + gentle leveling\n\
               for noisy rooms, keyboards, fans\n\
    wide       music-friendly — full background kept, no suppression\n\
  minicast set --mic-mode voice\n\
\n\
  No crackle by design: the chain stays at macOS-native 48 kHz end to end\n\
  (no 48→44.1 kHz resample), gaps over 100 ms are stretched softly instead\n\
  of hard-clipped, and a limiter stops mixed sources from clipping.\n\
\n\
  macOS Control Center mic modes (Standard / Voice Isolation / Wide\n\
  Spectrum) are per-app OS toggles — they apply on top when enabled for\n\
  your terminal while streaming. Use either those or --mic-mode, not both,\n\
  or the double processing sounds hollow.\n\
\n\
  System audio (game/desktop sound) needs a loopback driver such as BlackHole:\n\
    minicast set --system-audio 2        # by ID, like --mic\n\
    minicast set --system-audio off      # disable\n\
\n\
  Mic + system audio are mixed automatically (system sound is never\n\
  denoised). At least one source is required (`validate` enforces this).\n";

pub const CAMERA: &str = "\
CAMERA — PiP look presets, applied on the tiny PiP frame (near-free)\n\
\n\
  minicast set --camera-filter off        # raw sensor, zero processing\n\
  minicast set --camera-filter standard  # light denoise + touch of color (default)\n\
  minicast set --camera-filter studio    # studio-light-like: lifted, punchy, sharpened\n\
  minicast set --camera-filter lowlight  # stronger denoise + exposure lift for dim rooms\n\
\n\
  macOS Control Center video effects (Portrait blur, Studio Light,\n\
  Center Stage, Desk View) are per-app OS toggles — they apply underneath\n\
  when enabled for your terminal while streaming. Combine freely:\n\
  OS effect for framing/depth, --camera-filter for the picture itself.\n\
  If the PiP ever stutters, drop to --camera-filter off first.\n";

pub const SET_TOPIC: &str = "\
SET — every flag (all values are saved to config.toml)\n\
\n\
  Video source\n\
    --screen 1 | \"Capture screen 0\"   screen device (ID or name)\n\
    --screen-fit cover|contain          cover fills the frame (default, no bars);\n\
                                        contain letterboxes instead\n\
    --cursor true|false                 capture the mouse cursor (default true)\n\
    --camera true|false                 PiP on/off\n\
    --camera-device 0 | \"FaceTime\"     camera device (ID or name)\n\
    --camera-size 320x180                PiP size in pixels\n\
    --camera-pos top-left|top-right|bottom-left|bottom-right|custom\n\
    --camera-margin 20 | 20,20           distance from the corner(s)\n\
    --camera-filter off|standard|studio|lowlight   PiP look (see `help camera`)\n\
\n\
  Audio (IDs work everywhere — see `minicast help audio`)\n\
    --mic 1 | \"MacBook Air\" | off      microphone\n\
    --mic-mode standard|voice|wide      mic DSP: transparent | suppression | music\n\
    --system-audio 2 | \"BlackHole\" | off\n\
\n\
  Quality (see `minicast help quality`)\n\
    --resolution 1920x1080   --fps 30   --bitrate 6000   --encoder videotoolbox|x264\n\
\n\
  Destination & safety net\n\
    --rtmp rtmp://…                     stream destination (key included)\n\
    --record out.mp4 | off              local backup recording\n\
    --auto-reconnect true|false         restart on failure (default true)\n\
    --max-retries 10 | infinite         restart budget (default infinite)\n\
    --retry-delay 2                     first wait in seconds, doubles each time\n\
    --max-retry-delay 30                backoff ceiling in seconds\n\
\n\
  Run `minicast set` with no flags to print the current values.\n\
  Edits never touch a running stream — it keeps its start-time snapshot.\n";

pub const QUALITY: &str = "\
QUALITY — always full quality, tuned for live\n\
\n\
  Encoder: VideoToolbox (Apple hardware, default) — near-zero CPU, full\n\
  resolution and your exact bitrate held as CBR. x264 is the software fallback:\n\
    minicast set --encoder x264\n\
\n\
  Bitrate guide (H.264 CBR, YouTube/Twitch)\n\
    1280x720 @30    3500 kbps      1920x1080 @30    6000 kbps\n\
    1280x720 @60    5000 kbps      1920x1080 @60    9000 kbps\n\
\n\
  Auto-fps follows the captured display up to 60 (the platform ingest\n\
  max): a 60 Hz screen streams at 60, 100/120 Hz screens also resolve to\n\
  60 — platforms drop or reject anything higher. 60 fps needs ~2x the\n\
  bitrate of 30 fps at the same resolution (1080p60 -> ~9000 kbps), or\n\
  the platform reports a very low received bitrate even on excellent\n\
  upload. `validate` warns when the bitrate is thin. For a lighter stream\n\
  set --fps 30 explicitly.\n\
\n\
  What minicast guarantees on top of your settings:\n\
    • exact bitrate as target AND ceiling (CBR), 2 s keyframes (platform requirement)\n\
    • high profile + yuv420p for maximum player compatibility\n\
    • constant frame rate from the fps filters — dropped capture frames never change pace\n\
    • device capture timestamps (no wallclock skew), fast bilinear scaling\n\
    • VideoToolbox falls back to software instead of failing when HW is busy\n\
    • corrupt packets are discarded, timestamps rebuilt, audio resynced at 48 kHz\n\
\n\
  If your upload can't sustain the bitrate, lower --bitrate rather than\n\
  fighting it — a clean 720p beats a stuttering 1080p.\n";

pub const RETRY: &str = "\
RETRY — the stream does not end when the internet does\n\
\n\
  Two layers keep you live through lag, drops and short outages:\n\
\n\
  1. In-ffmpeg shock absorbers (always on): deep capture queues, burst-proof\n\
     muxing, drop-a-packet-instead-of-dying encoder policy, timestamp repair.\n\
     Sub-second hiccups are invisible to viewers.\n\
\n\
  2. Auto-restart with exponential backoff (default on): if ffmpeg itself\n\
     exits with an error, minicast waits 2 s, then 4 s, 8 s … (cap 30 s) and\n\
     restarts — forever, unless you cap it. Works identically in the\n\
     foreground (`minicast start`) and in the background (`--detach`, where a\n\
     supervisor shell owns the loop and `stream stop` kills the whole tree).\n\
\n\
  Tune it:\n\
    minicast set --auto-reconnect false     # one shot, no restarts\n\
    minicast set --max-retries 10           # give up after 10 restarts\n\
    minicast set --max-retries infinite     # back to forever (default)\n\
    minicast set --retry-delay 2 --max-retry-delay 30\n\
    minicast start --no-retry               # one-shot override for this run\n\
    minicast start --max-retries 5          # override the budget for this run\n\
\n\
  Watch retries happen:  minicast stream logs <name>\n\
  A clean exit (code 0) or Ctrl-C never retries — only failures do.\n";

pub const RECORD: &str = "\
RECORD — local backup of every stream\n\
\n\
  minicast set --record ~/Movies/show.mp4   # record alongside streaming\n\
  minicast set --record off                 # disable\n\
\n\
  The backup is a separate small encode, so it never affects stream quality.\n\
  If the network dies mid-show, the recording still has everything.\n";

pub const START_TOPIC: &str = "\
START — foreground vs detached\n\
\n\
  minicast start                   foreground: logs to your terminal,\n\
                                   Ctrl-C stops. Failures auto-retry.\n\
  minicast start --detach --name yt\n\
                                   background: survives terminal exit, tracked\n\
                                   in `stream list`, supervised with auto-retry.\n\
  minicast start --dry-run         print the ffmpeg command without running it.\n\
  minicast start --no-retry        this run only: exit on first failure.\n\
  minicast start --max-retries 5   this run only: cap the restart budget.\n\
\n\
  Config is snapshotted at start — editing config.toml mid-stream never\n\
  affects the running stream. Manage detached runs with:\n\
    minicast stream list | minicast stream logs yt | minicast stream stop yt\n";

pub const TROUBLESHOOT: &str = "\
TROUBLESHOOTING\n\
\n\
  `set` says a device was not found\n\
    IDs shift when hardware changes. Run `minicast devices` and use fresh IDs.\n\
\n\
  ffmpeg exits after ~1 s, retrying fast\n\
    Usually a bad device or a rejected key. Run `minicast validate`, check the\n\
    log (`minicast stream logs <name>` or the terminal), verify the RTMP URL.\n\
    YouTube keys expire — generate a fresh one in Studio.\n\
\n\
  Viewers see buffering but the Mac is fine\n\
    Upload is the bottleneck. Drop --bitrate a notch (see `help quality`) or\n\
    stream 720p. Retries in the log with stable uptime = network, not minicast.\n\
\n\
  No audio / \"set at least one of mic_device or system_device\"\n\
    minicast set --mic 1   (pick the ID from `minicast devices`)\n\
\n\
  Camera PiP missing / wrong corner\n\
    minicast set --camera true --camera-device 0 --camera-pos bottom-right\n\
\n\
  Camera or screen looks laggy / stuttery / half-speed on stream\n\
    That is almost always the encode falling behind, not the network:\n\
    duplicated frames compress to nothing, so the platform ALSO reports a\n\
    very low bitrate even on excellent upload. Fix in order:\n\
    1. Check `minicast validate` for a thin-bitrate warning and match\n\
       bitrate to resolution/fps (`help quality`) so CBR can hold it —\n\
       1080p60 needs ~9000 kbps, 1080p30 ~6000.\n\
    2. Close heavy apps, try --camera-filter off.\n\
    True network buffering freezes instead of slowing — retries in the log\n\
    with stable uptime = network, half-speed motion = encode load.\n\
\n\
  Mic crackles, clicks or sounds crushed\n\
    Stay on one processing layer: use --mic-mode voice OR the macOS Control\n\
    Center Voice Isolation for the app, not both. If the mix clips (mic +\n\
    system audio loud together), that is handled by the limiter — but check\n\
    the source volumes in Sound settings as well.\n\
\n\
  ffmpeg not found\n\
    brew install ffmpeg   (needs avfoundation support — Homebrew's build has it)\n\
\n\
  Still stuck? `minicast start --dry-run` shows the exact ffmpeg command;\n\
  run it by hand to see ffmpeg's own error.\n";

/// Guide text for `help <topic>`. Returns None for real subcommand names
/// (those render through clap instead).
pub fn guide_topic(topic: &str) -> Option<&'static str> {
    match topic.to_lowercase().as_str() {
        "quickstart" | "start-here" | "begin" => Some(QUICKSTART),
        "youtube" | "key" | "twitch" => Some(YOUTUBE),
        "devices" => Some(DEVICES_TOPIC),
        "audio" | "mic" | "microphone" | "sound" | "denoise" | "suppression" => Some(AUDIO),
        "camera" | "cam" | "pip" | "filter" | "filters" | "portrait" => Some(CAMERA),
        "set" => Some(SET_TOPIC),
        "quality" | "bitrate" | "encoder" | "1080p" | "fps" => Some(QUALITY),
        "retry" | "retries" | "reconnect" | "resilience" | "network" | "lag" | "drops" => {
            Some(RETRY)
        }
        "record" | "recording" | "backup" => Some(RECORD),
        "start" | "detach" | "foreground" => Some(START_TOPIC),
        "troubleshoot" | "troubleshooting" | "faq" | "errors" | "stuck" => {
            Some(TROUBLESHOOT)
        }
        "help" | "guide" | "howto" | "all" => Some(OVERVIEW),
        _ => None,
    }
}

/// True for real subcommand names (including the config/stream nested forms).
pub fn is_command(topic: &str) -> bool {
    matches!(
        topic.to_lowercase().as_str(),
        "init"
            | "devices"
            | "config"
            | "config show"
            | "config path"
            | "set"
            | "validate"
            | "start"
            | "stream"
            | "stream list"
            | "stream stop"
            | "stream logs"
            | "help"
    )
}
