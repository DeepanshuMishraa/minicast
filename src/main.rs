// minicast — macOS-only minimal livestreaming CLI.
#[cfg(not(target_os = "macos"))]
compile_error!("minicast is macOS-only for now (AVFoundation + VideoToolbox)");

mod config;
mod devices;
mod ffmpeg;
mod help;
mod mic;
mod streams;
mod uplink;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "minicast",
    version,
    about = "Minimal macOS livestreaming CLI: screen + camera PiP + mic -> RTMP",
    long_about = "Minimal macOS livestreaming CLI: screen + camera PiP + mic -> RTMP.\n\nRun `minicast help` for the full guide with examples, or\n`minicast help <topic>` for a deep dive (quickstart, youtube, audio,\nquality, retry, troubleshoot).",
    after_help = "Examples:\n  minicast help quickstart\n  minicast devices\n  minicast set --screen 1 --camera-device 0 --mic 1 --rtmp rtmp://a.rtmp.youtube.com/live2/KEY",
    disable_help_subcommand = true
)]
struct Cli {
    /// Config file. Defaults to ~/.config/minicast/config.toml
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write default config (to ~/.config/minicast/config.toml unless -c given)
    #[command(long_about = "Write a default config file and exit.\n\nThe defaults stream 1080p30 via hardware encoding; you still need to\npick devices and an RTMP destination:\n\n  minicast init\n  minicast devices\n  minicast set --screen 1 --camera-device 0 --mic 1 \\\n      --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY\n\nUse --force to reset an existing config back to defaults.")]
    Init {
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// List cameras, screens, microphones (via ffmpeg avfoundation)
    #[command(long_about = "Probe macOS AVFoundation devices and list them with numeric IDs.\n\nUse the bracketed IDs with `set` — screen, camera AND mic all accept\nan ID or a name:\n\n  minicast set --screen 1 --camera-device 0 --mic 1\n  minicast set --mic \"MacBook Air Microphone\"\n\nRe-run after plugging hardware in/out: macOS renumbers IDs when\ndevices appear or disappear.")]
    Devices,
    /// Show config file contents and path
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Set one or more config values (saved to config file). Run without flags to see current values.
    #[command(long_about = "Set one or more config values (saved to config.toml).\n\nScreen, camera AND mic each accept a numeric ID (from `devices`) or a\nname/substring, e.g. --screen 1, --camera-device 0, --mic 1.\nRun with no flags to print the current values.\n\nExamples:\n  minicast set --screen 1 --camera-device 0 --mic 1 \\\n      --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY\n  minicast set --resolution 1920x1080 --fps 30 --bitrate 6000\n  minicast set --camera-pos top-right --camera-size 320x180\n  minicast set --camera-filter studio --mic-mode voice\n  minicast set --mic off --system-audio 2\n  minicast set --auto-reconnect true --max-retries infinite\n\nFull flag reference: `minicast help set`.")]
    Set {
        /// Screen device index or name, e.g. "1" or "Capture screen 0"
        #[arg(long)]
        screen: Option<String>,
        /// Screen fit: cover (fill, crop, no black bars) | contain (letterbox)
        #[arg(long)]
        screen_fit: Option<String>,
        /// Capture cursor: true | false
        #[arg(long)]
        cursor: Option<String>,
        /// Camera on/off: true | false
        #[arg(long)]
        camera: Option<String>,
        /// Camera device index or name, e.g. "0" or "FaceTime"
        #[arg(long)]
        camera_device: Option<String>,
        /// Camera PiP size, e.g. 320x180
        #[arg(long)]
        camera_size: Option<String>,
        /// Camera corner: top-left|top-right|bottom-left|bottom-right|custom
        #[arg(long)]
        camera_pos: Option<String>,
        /// Camera margin in px, e.g. 20 or 20,20
        #[arg(long)]
        camera_margin: Option<String>,
        /// Camera look: off | standard | studio | lowlight
        #[arg(long)]
        camera_filter: Option<String>,
        /// Mic device index or name, e.g. "1" or "MacBook Air Microphone". Use "off" to disable.
        #[arg(long)]
        mic: Option<String>,
        /// Mic DSP preset: standard | voice (suppression) | wide (music)
        #[arg(long)]
        mic_mode: Option<String>,
        /// System audio device index or name (needs BlackHole etc). Use "off" to disable.
        #[arg(long)]
        system_audio: Option<String>,
        /// Output resolution, e.g. 1920x1080
        #[arg(long)]
        resolution: Option<String>,
        /// FPS: a number (e.g. 30, 60) or "auto" to follow the display's refresh rate (max 60)
        #[arg(long)]
        fps: Option<String>,
        /// Video bitrate in kbps (e.g. 6000, pins it) or "auto" (default: measured from your uplink at start)
        #[arg(long)]
        bitrate: Option<String>,
        /// Encoder: videotoolbox (hw, default) | x264
        #[arg(long)]
        encoder: Option<String>,
        /// RTMP URL (stream key included)
        #[arg(long)]
        rtmp: Option<String>,
        /// Local record path. Use "off" to disable.
        #[arg(long)]
        record: Option<String>,
        /// Auto-restart on failure (net loss etc): true | false
        #[arg(long)]
        auto_reconnect: Option<String>,
        /// Restart budget: number, or "infinite" (default)
        #[arg(long)]
        max_retries: Option<String>,
        /// First wait between restarts in seconds (doubles each attempt)
        #[arg(long)]
        retry_delay: Option<u64>,
        /// Backoff ceiling in seconds
        #[arg(long)]
        max_retry_delay: Option<u64>,
    },
    /// Validate the config file
    #[command(long_about = "Validate the config file without streaming.\n\nChecks the RTMP URL, resolution/fps, mic presence, encoder, PiP\nplacement and retry settings, then prints the resolved summary.\nRun this before going live.")]
    Validate,
    /// Start streaming (spawns a single tuned ffmpeg process).
    /// Config is snapshotted at start — later edits never affect a running stream.
    #[command(long_about = "Start streaming. Config is snapshotted at start — later edits never\naffect a running stream.\n\n  minicast start                    foreground; Ctrl-C stops\n  minicast start --detach --name yt   background; survives terminal exit\n  minicast start --dry-run            print the ffmpeg command only\n\nFailures auto-retry with backoff (see `minicast help retry`):\n  minicast start --no-retry           exit on first failure (this run)\n  minicast start --max-retries 5      cap restarts (this run)")]
    Start {
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Run in background and track it under `stream list`
        #[arg(long, short, default_value_t = false)]
        detach: bool,
        /// Name for the detached stream (default stream-<id>)
        #[arg(long)]
        name: Option<String>,
        /// This run only: exit on first failure instead of retrying
        #[arg(long, default_value_t = false)]
        no_retry: bool,
        /// This run only: cap auto-restarts (overrides config max-retries)
        #[arg(long)]
        max_retries: Option<u32>,
    },
    /// Manage running live streams
    #[command(long_about = "Manage detached streams:\n\n  minicast stream list          show running streams + uptime\n  minicast stream logs yt       tail the stream log (retries included)\n  minicast stream stop yt       stop one stream\n  minicast stream stop --all    stop everything")]
    Stream {
        #[command(subcommand)]
        action: StreamAction,
    },
    /// Show the detailed guide (topics: quickstart, youtube, audio, quality, retry, …)
    #[command(visible_alias = "guide", long_about = "Show the detailed minicast guide.\n\n  minicast help                 overview + quickstart\n  minicast help all             everything, start to finish\n  minicast help quickstart|youtube|devices|audio|set|quality|retry|record|start|troubleshoot\n  minicast help <command>       clap reference for init/devices/config/set/validate/start/stream")]
    Help {
        /// Topic or command name (use \"all\" for the complete guide)
        topic: Option<String>,
    },
}

#[derive(Subcommand)]
enum StreamAction {
    /// Show running live streams (platform, destination, uptime)
    List,
    /// Stop a running stream by id or name
    Stop {
        /// Stream id or name (from `stream list`)
        id: Option<String>,
        /// Stop all running streams
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Tail a stream's log (ffmpeg output + supervisor retry lines)
    Logs {
        /// Stream id or name (default: the only running stream)
        id: Option<String>,
        /// How many log lines to show
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print config path and contents
    Show,
    /// Print only the config path
    Path,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = config::resolve_config_path(cli.config);

    match cli.cmd {
        Cmd::Init { force } => {
            if cfg_path.exists() && !force {
                bail!("{} already exists (use --force to overwrite)", cfg_path.display());
            }
            let cfg = config::Config::default();
            cfg.save(&cfg_path)?;
            println!("wrote {}", cfg_path.display());
            println!("next: minicast devices  # find your screen/camera/mic IDs");
            println!("then: minicast set --screen 1 --camera-device 0 --mic 1 --rtmp rtmp://a.rtmp.youtube.com/live2/YOUR-KEY");
            println!("guide: minicast help quickstart");
        }
        Cmd::Devices => {
            let list = devices::probe_avfoundation()?;
            println!("screens (use with --screen <id>):");
            for (i, name) in list.screens() {
                println!("  [{i}] {name}");
            }
            println!("cameras (use with --camera-device <id>):");
            for (i, name) in list.cameras() {
                println!("  [{i}] {name}");
            }
            println!("audio devices (use with --mic <id> / --system-audio <id>):");
            for (i, name) in &list.audio {
                println!("  [{i}] {name}");
            }
            if list.video.is_empty() && list.audio.is_empty() {
                println!("(no devices found — is ffmpeg with avfoundation installed? try: brew install ffmpeg)");
            } else {
                println!("tip: minicast set --screen <id> --camera-device <id> --mic <id>");
            }
        }
        Cmd::Config { action } => match action {
            ConfigAction::Path => println!("{}", cfg_path.display()),
            ConfigAction::Show => {
                println!("config: {}", cfg_path.display());
                if !cfg_path.exists() {
                    println!("(missing — run `minicast init` to create it)");
                    return Ok(());
                }
                let text = std::fs::read_to_string(&cfg_path)
                    .with_context(|| format!("failed to read {}", cfg_path.display()))?;
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
        },
        Cmd::Set {
            screen,
            screen_fit,
            cursor,
            camera,
            camera_device,
            camera_size,
            camera_pos,
            camera_margin,
            camera_filter,
            mic,
            mic_mode,
            system_audio,
            resolution,
            fps,
            bitrate,
            encoder,
            rtmp,
            record,
            auto_reconnect,
            max_retries,
            retry_delay,
            max_retry_delay,
        } => {
            let no_flags = screen.is_none()
                && screen_fit.is_none()
                && cursor.is_none()
                && camera.is_none()
                && camera_device.is_none()
                && camera_size.is_none()
                && camera_pos.is_none()
                && camera_margin.is_none()
                && camera_filter.is_none()
                && mic.is_none()
                && mic_mode.is_none()
                && system_audio.is_none()
                && resolution.is_none()
                && fps.is_none()
                && bitrate.is_none()
                && encoder.is_none()
                && rtmp.is_none()
                && record.is_none()
                && auto_reconnect.is_none()
                && max_retries.is_none()
                && retry_delay.is_none()
                && max_retry_delay.is_none();
            let mut cfg = config::Config::load_or_init(&cfg_path)?;

            if no_flags {
                // Best-effort device names (no probe failure if ffmpeg is missing).
                let list = devices::probe_avfoundation().ok();
                print_current(&cfg, &cfg_path, list.as_ref());
                return Ok(());
            }

            // Validate device queries against live device list when changing them.
            // Screen, camera and mic all behave identically: numeric ID or name.
            let mut probed: Option<devices::DeviceList> = None;
            if screen.is_some() || camera_device.is_some() || mic.is_some() || system_audio.is_some()
            {
                let list = devices::probe_avfoundation()?;
                if let Some(q) = screen.as_deref() {
                    let idx = list.resolve_video(q).ok_or_else(|| {
                        anyhow::anyhow!(
                            "screen {q:?} not found. Screens: {}",
                            list.screens()
                                .iter()
                                .map(|(i, n)| format!("[{i}] {n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    cfg.screen.video_device = list.video_name(&idx).unwrap_or(&idx).to_string();
                }
                if let Some(q) = camera_device.as_deref() {
                    let idx = list.resolve_video(q).ok_or_else(|| {
                        anyhow::anyhow!(
                            "camera {q:?} not found. Cameras: {}",
                            list.cameras()
                                .iter()
                                .map(|(i, n)| format!("[{i}] {n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    cfg.camera.video_device = list.video_name(&idx).unwrap_or(&idx).to_string();
                }
                if let Some(q) = mic.as_deref() {
                    if q.eq_ignore_ascii_case("off") || q.trim().is_empty() {
                        cfg.audio.mic_device.clear();
                    } else {
                        // Stored as the numeric ID, exactly like --screen/--camera-device.
                        let idx = list.resolve_audio(q).ok_or_else(|| {
                            anyhow::anyhow!(
                                "mic {q:?} not found. Audio: {}",
                                list.audio
                                    .iter()
                                    .map(|(i, n)| format!("[{i}] {n}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })?;
                        cfg.audio.mic_device = list.audio_name(&idx).unwrap_or(&idx).to_string();
                    }
                }
                if let Some(q) = system_audio.as_deref() {
                    if q.eq_ignore_ascii_case("off") || q.trim().is_empty() {
                        cfg.audio.system_device.clear();
                    } else {
                        let idx = list.resolve_audio(q).ok_or_else(|| {
                            anyhow::anyhow!("system audio {q:?} not found (hint: install BlackHole for system capture)")
                        })?;
                        cfg.audio.system_device = list.audio_name(&idx).unwrap_or(&idx).to_string();
                    }
                }
                probed = Some(list);
            }

            if let Some(v) = screen_fit.as_deref() {
                match v.to_lowercase().as_str() {
                    "cover" | "contain" => cfg.screen.fit = v.to_lowercase(),
                    _ => bail!("--screen-fit must be cover|contain"),
                }
            }
            if let Some(v) = cursor.as_deref() {
                cfg.screen.capture_cursor = parse_bool(v)
                    .ok_or_else(|| anyhow::anyhow!("--cursor must be true|false|on|off|1|0"))?;
            }
            if let Some(v) = camera.as_deref() {
                cfg.camera.enabled = parse_bool(v)
                    .ok_or_else(|| anyhow::anyhow!("--camera must be true|false|on|off|1|0"))?;
            }
            if let Some(v) = camera_size.as_deref() {
                let (cw, ch) = parse_dims(v)
                    .ok_or_else(|| anyhow::anyhow!("--camera-size must look like 320x180"))?;
                cfg.camera.width = cw;
                cfg.camera.height = ch;
            }
            if let Some(v) = camera_pos.as_deref() {
                match v.to_lowercase().as_str() {
                    "top-left" | "top-right" | "bottom-left" | "bottom-right" | "custom" => {
                        cfg.camera.corner = v.to_lowercase()
                    }
                    _ => bail!("--camera-pos must be top-left|top-right|bottom-left|bottom-right|custom"),
                }
            }
            if let Some(v) = camera_margin.as_deref() {
                let (mx, my) = parse_margin(v)
                    .ok_or_else(|| anyhow::anyhow!("--camera-margin must be like 20 or 20,20"))?;
                cfg.camera.margin_x = mx;
                cfg.camera.margin_y = my;
            }
            if let Some(v) = camera_filter.as_deref() {
                match v.to_lowercase().as_str() {
                    "off" | "standard" | "studio" | "lowlight" => {
                        cfg.camera.filter = v.to_lowercase()
                    }
                    _ => bail!("--camera-filter must be off|standard|studio|lowlight"),
                }
            }
            if let Some(v) = mic_mode.as_deref() {
                match v.to_lowercase().as_str() {
                    "standard" | "voice" | "wide" => {
                        cfg.audio.mic_mode = v.to_lowercase()
                    }
                    _ => bail!("--mic-mode must be standard|voice|wide"),
                }
            }
            if let Some(v) = resolution.as_deref() {
                let (w, h) = parse_dims(v)
                    .ok_or_else(|| anyhow::anyhow!("--resolution must look like 1920x1080"))?;
                cfg.video.width = w;
                cfg.video.height = h;
            }
            if let Some(v) = fps.as_deref() {
                if v.eq_ignore_ascii_case("auto") {
                    cfg.video.fps_auto = true;
                } else {
                    cfg.video.fps = v
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--fps must be a number or \"auto\", got {v:?}"))?;
                    cfg.video.fps_auto = false;
                }
            }
            if let Some(v) = bitrate.as_deref() {
                if v.eq_ignore_ascii_case("auto") {
                    cfg.video.bitrate_auto = true;
                } else {
                    cfg.video.bitrate_kbps = v
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--bitrate must be a number (kbps) or \"auto\", got {v:?}"))?;
                    cfg.video.bitrate_auto = false;
                }
            }
            if let Some(v) = encoder.as_deref() {
                match v.to_lowercase().as_str() {
                    "videotoolbox" | "x264" => cfg.video.encoder = v.to_lowercase(),
                    _ => bail!("--encoder must be videotoolbox|x264"),
                }
            }
            if let Some(v) = rtmp {
                cfg.output.rtmp_url = v;
            }
            if let Some(v) = record.as_deref() {
                if v.eq_ignore_ascii_case("off") || v.trim().is_empty() {
                    cfg.output.record_path = None;
                } else {
                    cfg.output.record_path = Some(v.to_string());
                }
            }
            if let Some(v) = auto_reconnect.as_deref() {
                cfg.output.auto_reconnect = parse_bool(v)
                    .ok_or_else(|| anyhow::anyhow!("--auto-reconnect must be true|false|on|off|1|0"))?;
            }
            if let Some(v) = max_retries.as_deref() {
                cfg.output.max_retries = parse_max_retries(v)
                    .ok_or_else(|| anyhow::anyhow!("--max-retries must be a number or \"infinite\" (use --auto-reconnect false to disable retries)"))?;
            }
            if let Some(v) = retry_delay {
                cfg.output.retry_delay_secs = v;
            }
            if let Some(v) = max_retry_delay {
                cfg.output.max_retry_delay_secs = v;
            }

            cfg.validate()?;
            cfg.save(&cfg_path)?;
            println!("saved {}", cfg_path.display());
            print_current(&cfg, &cfg_path, probed.as_ref());
            if let Ok(live) = streams::alive_records() {
                if !live.is_empty() {
                    println!(
                        "note: {} live stream(s) running — changes apply to the next start; \
                         running streams keep their start-time snapshot.",
                        live.len()
                    );
                }
            }
        }
        Cmd::Validate => {
            let mut cfg = config::Config::load(&cfg_path)?;
            ffmpeg::resolve_media(&mut cfg)?;
            let (x, y) = cfg.camera_xy();
            let retry = streams::RetryPolicy::from_config(&cfg);
            println!("{} is valid", cfg_path.display());
            println!("encoder: {}", ffmpeg::resolved_encoder(&cfg));
            println!("fps: {}{}", cfg.video.fps, if cfg.video.fps_auto { " (auto: display refresh, max 60 = platform ingest limit)" } else { "" });
            if let Some((w, h, f)) = cfg.camera.capture_mode {
                println!("camera mode: {w}x{h}@{f}");
            }
            println!("screen: device {} fit={} cursor={}", cfg.screen.video_device, cfg.screen.fit, cfg.screen.capture_cursor);
            if cfg.camera.enabled {
                println!("camera PiP: {}x{} at {},{} filter={}", cfg.camera.width, cfg.camera.height, x, y, cfg.camera.filter);
            }
            println!("mic: {} mode (dsp: {})", if cfg.audio.mic_device.trim().is_empty() { "(off)" } else { &cfg.audio.mic_device }, cfg.audio.mic_mode);
            println!(
                "retry: {}",
                if retry.enabled {
                    match retry.max_retries {
                        Some(m) => format!("on, up to {m} restarts ({}s → {}s backoff)", retry.base_secs, retry.cap_secs),
                        None => format!("on, infinite restarts ({}s → {}s backoff)", retry.base_secs, retry.cap_secs),
                    }
                } else {
                    "off (one shot — any failure ends the stream)".to_string()
                }
            );
            if let Some(hint) = bitrate_hint(&cfg) {
                println!("warning: {hint}");
            }
            println!("destination: {} [{}]",
                streams::mask_rtmp_url(&cfg.output.rtmp_url),
                streams::detect_platform(&cfg.output.rtmp_url));
        }
        Cmd::Start { dry_run, detach, name, no_retry, max_retries } => {
            let mut cfg = config::Config::load(&cfg_path)?;
            ffmpeg::resolve_media(&mut cfg)?;
            if dry_run {
                let argv = ffmpeg::build_argv(&cfg)?;
                println!("ffmpeg {}", argv.join(" "));
                return Ok(());
            }
            // Snapshot argv up front: everything after this point runs off the
            // snapshot, so editing config.toml mid-stream is always safe.
            let argv = ffmpeg::build_argv(&cfg)?;
            if let Some(hint) = bitrate_hint(&cfg) {
                println!("warning: {hint}");
            }
            // Retry policy: config defaults, `start` flags win for this run.
            let mut retry = streams::RetryPolicy::from_config(&cfg);
            if no_retry {
                retry.enabled = false;
            }
            if let Some(m) = max_retries {
                retry.enabled = true;
                retry.max_retries = Some(m);
            }
            if detach {
                let rec = streams::spawn_detached(name.as_deref(), &cfg, &argv, retry, cfg.audio.mic_capture.as_ref())?;
                println!(
                    "started stream {} ({}) -> {} [{}]{}",
                    rec.id,
                    rec.name,
                    rec.platform,
                    streams::mask_rtmp_url(&rec.rtmp_url),
                    if retry.enabled { " [auto-retry on]" } else { " [one shot]" },
                );
                println!("config snapshotted to {}", rec.snapshot_path);
                println!("log: {}", rec.log_path);
                println!("manage with: minicast stream list | minicast stream logs {} | minicast stream stop {}", rec.id, rec.id);
            } else {
                println!(
                    "streaming to {} [{}] (Ctrl-C to stop) …",
                    streams::mask_rtmp_url(&cfg.output.rtmp_url),
                    streams::detect_platform(&cfg.output.rtmp_url)
                );
                println!(
                    "snapshot taken — editing {} won't affect this stream.",
                    cfg_path.display()
                );
                if retry.enabled {
                    match retry.max_retries {
                        Some(m) => println!("auto-retry on: up to {m} restarts ({}s → {}s backoff).", retry.base_secs, retry.cap_secs),
                        None => println!("auto-retry on: infinite restarts ({}s → {}s backoff). Ctrl-C stops.", retry.base_secs, retry.cap_secs),
                    }
                }
                run_foreground(&argv, cfg.audio.mic_capture.as_ref(), retry)?;
            }
        }
        Cmd::Stream { action } => match action {
            StreamAction::List => {
                let (alive, pruned) = streams::prune_dead()?;
                if alive.is_empty() {
                    println!("no live streams{}", if pruned > 0 { format!(" (cleaned up {pruned} exited)") } else { String::new() });
                    return Ok(());
                }
                println!(
                    "{:<4} {:<14} {:<10} {:<7} {:<7} {:<12} {}",
                    "ID", "NAME", "PLATFORM", "PID", "UPTIME", "OUTPUT", "DESTINATION"
                );
                let now = streams::unix_now();
                for r in &alive {
                    println!(
                        "{:<4} {:<14} {:<10} {:<7} {:<7} {:<12} {}",
                        r.id,
                        truncate(&r.name, 14),
                        r.platform,
                        r.pid,
                        streams::format_uptime(r.started_at, now),
                        r.output,
                        streams::mask_rtmp_url(&r.rtmp_url)
                    );
                }
                if pruned > 0 {
                    println!("(cleaned up {pruned} exited stream(s))");
                }
                println!("config edits never affect running streams (each start snapshots config).");
                println!("supervised streams auto-retry on failure — watch with: minicast stream logs <id|name>");
            }
            StreamAction::Stop { id, all } => {
                if all {
                    let alive = streams::alive_records()?;
                    if alive.is_empty() {
                        println!("no live streams");
                        return Ok(());
                    }
                    for r in &alive {
                        let stopped = if r.supervised {
                            streams::terminate_tree(r.pid)?
                        } else {
                            streams::terminate(r.pid)?
                        };
                        if stopped {
                            println!("stopped stream {} ({})", r.id, r.name);
                        } else {
                            println!("could not stop stream {} ({}) pid {}", r.id, r.name, r.pid);
                        }
                        streams::remove_record(&r.id)?;
                    }
                    return Ok(());
                }
                let target = id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("usage: minicast stream stop <id|name>  (or --all)"))?;
                let rec = streams::remove_record(target)?
                    .ok_or_else(|| anyhow::anyhow!("no stream {target:?} (see `minicast stream list`)"))?;
                if streams::is_alive(rec.pid) {
                    let stopped = if rec.supervised {
                        streams::terminate_tree(rec.pid)?
                    } else {
                        streams::terminate(rec.pid)?
                    };
                    if stopped {
                        println!("stopped stream {} ({}) [{}]", rec.id, rec.name, rec.platform);
                    } else {
                        println!(
                            "stream {} ({}) removed but pid {} would not die — check manually",
                            rec.id, rec.name, rec.pid
                        );
                    }
                } else {
                    println!("stream {} ({}) had already exited — record removed", rec.id, rec.name);
                }
            }
            StreamAction::Logs { id, lines } => {
                let rec = match id.as_deref() {
                    Some(target) => streams::find_record(target)?
                        .ok_or_else(|| anyhow::anyhow!("no stream {target:?} (see `minicast stream list`)"))?,
                    None => {
                        let alive = streams::alive_records()?;
                        match alive.len() {
                            0 => bail!("no live streams (see `minicast stream list`)"),
                            1 => alive.into_iter().next().expect("exactly one"),
                            _ => bail!("multiple live streams — specify one: minicast stream logs <id|name>"),
                        }
                    }
                };
                println!("log: {}", rec.log_path);
                print!("{}", streams::tail_log(&rec.log_path, lines)?);
            }
        },
        Cmd::Help { topic } => {
            match topic.as_deref().map(str::trim) {
                None | Some("") => print!("{}", help::OVERVIEW),
                Some(t) if t.eq_ignore_ascii_case("all") => print_full_guide(),
                Some(t) => {
                    let mut shown = false;
                    // Command reference first (e.g. `help set`).
                    let first = t.split_whitespace().next().unwrap_or(t).to_lowercase();
                    if help::is_command(&t.to_lowercase()) || help::is_command(&first) {
                        if render_command_help(&first) {
                            shown = true;
                        }
                    }
                    if let Some(g) = help::guide_topic(t) {
                        if shown {
                            println!();
                            println!("--- guide ---");
                        }
                        print!("{g}");
                        // OVERVIEW already is the full command table; don't repeat it.
                        if !std::ptr::eq(g, help::OVERVIEW) {
                            println!("\nSee also: minicast help all | minicast help troubleshoot");
                        }
                        shown = true;
                    }
                    if !shown {
                        println!("unknown topic {t:?}.\n");
                        print!("{}", help::OVERVIEW);
                    }
                }
            }
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// Foreground run with auto-retry: a clean exit (code 0) or a signal death
/// stops the loop; any other failure sleeps with backoff and restarts.
fn run_foreground(argv: &[String], mic: Option<&mic::MicCapture>, retry: streams::RetryPolicy) -> Result<()> {
    let mut attempt: u32 = 0;
    loop {
        let started = streams::unix_now();
        let status = ffmpeg::run_once(argv, mic)?;
        if status.success() {
            println!("ffmpeg exited cleanly — stream over.");
            return Ok(());
        }
        if status.code().is_none() {
            // Killed by a signal (e.g. Ctrl-C reached ffmpeg first): respect it.
            println!("ffmpeg stopped by signal — not retrying.");
            return Ok(());
        }
        if !retry.enabled {
            println!("ffmpeg exited with {status} (auto-reconnect off) — stream over.");
            return Ok(());
        }
        attempt += 1;
        if !retry.budget_left(attempt) {
            println!("ffmpeg exited with {status}; retry budget exhausted after {} attempt(s) — stream over.", attempt - 1);
            return Ok(());
        }
        let uptime = streams::unix_now().saturating_sub(started);
        let delay = retry.delay_for(attempt);
        if uptime < 5 {
            println!("ffmpeg exited after {uptime}s with {status} — failing fast.");
            println!("hint: run `minicast validate`, check devices/keys, then retry.");
        }
        println!("retry {attempt} in {delay}s (Ctrl-C to stop) …");
        std::thread::sleep(std::time::Duration::from_secs(delay));
    }
}

/// Render clap's reference for one subcommand. Returns false when unknown.
fn render_command_help(name: &str) -> bool {
    let app = Cli::command();
    let found = app.find_subcommand(name).is_some();
    if !found {
        return false;
    }
    let mut sub = app.find_subcommand(name).expect("checked").clone();
    let mut buf = Vec::new();
    // Long help includes long_about examples; fall back to short help.
    if sub.write_long_help(&mut buf).is_ok() || {
        buf.clear();
        sub.write_help(&mut buf).is_ok()
    } {
        print!("{}", String::from_utf8_lossy(&buf));
        true
    } else {
        false
    }
}

fn print_full_guide() {
    print!("{}", help::OVERVIEW);
    for section in [
        help::QUICKSTART,
        help::YOUTUBE,
        help::DEVICES_TOPIC,
        help::AUDIO,
        help::CAMERA,
        help::SET_TOPIC,
        help::QUALITY,
        help::RETRY,
        help::RECORD,
        help::START_TOPIC,
        help::TROUBLESHOOT,
    ] {
        println!();
        println!("================================================================");
        print!("{section}");
    }
}

/// Warn when the bitrate is thin for the resolution+fps: bits per pixel per
/// frame below ~0.05 starves the encoder (blocky/fast-motion mush) and the
/// platform reports a low received bitrate. 60 fps needs ~2x the bitrate
/// of 30 fps at the same resolution.
fn bitrate_hint(cfg: &config::Config) -> Option<String> {
    let bpp = cfg.video.bitrate_kbps as f64 * 1000.0
        / (cfg.video.width as f64 * cfg.video.height as f64 * cfg.video.fps as f64);
    if bpp < 0.05 {
        let suggested = ((cfg.video.width as f64 * cfg.video.height as f64 * cfg.video.fps as f64
            * 0.07
            / 1000.0)
            / 500.0)
            .round() as u32
            * 500;
        Some(format!(
            "bitrate {} kbps is thin for {}x{}@{} (~{bpp:.3} bits/px/frame). \
             Try `minicast set --bitrate {suggested}` (see `minicast help quality`).",
            cfg.video.bitrate_kbps, cfg.video.width, cfg.video.height, cfg.video.fps,
            bpp = bpp,
            suggested = suggested,
        ))
    } else {
        None
    }
}

fn parse_bool(v: &str) -> Option<bool> {    match v.to_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Some(true),
        "false" | "0" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Parse `--max-retries`: a number caps restarts, "infinite" retries forever.
fn parse_max_retries(v: &str) -> Option<Option<u32>> {
    match v.trim().to_lowercase().as_str() {
        "infinite" | "inf" | "none" | "forever" | "unlimited" => Some(None),
        s => s.parse::<u32>().ok().map(Some),
    }
}

fn parse_dims(v: &str) -> Option<(u32, u32)> {
    let v = v.to_lowercase().replace('×', "x");
    let mut it = v.split('x');
    let w: u32 = it.next()?.trim().parse().ok()?;
    let h: u32 = it.next()?.trim().parse().ok()?;
    if it.next().is_some() || w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

fn parse_margin(v: &str) -> Option<(i32, i32)> {
    let v = v.replace(' ', "");
    if let Some((a, b)) = v.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        let m: i32 = v.parse().ok()?;
        Some((m, m))
    }
}

/// "1" -> "1 (MacBook Air Microphone)" when the device list is available.
fn display_audio(list: Option<&devices::DeviceList>, idx: &str) -> String {
    if idx.trim().is_empty() {
        return "(off)".to_string();
    }
    match list.and_then(|l| l.audio_name(idx)) {
        Some(name) => format!("{idx} ({name})"),
        None => idx.to_string(),
    }
}

fn display_video(list: Option<&devices::DeviceList>, idx: &str) -> String {
    match list.and_then(|l| l.video_name(idx)) {
        Some(name) => format!("{idx} ({name})"),
        None => idx.to_string(),
    }
}

fn print_current(cfg: &config::Config, path: &std::path::Path, list: Option<&devices::DeviceList>) {
    println!("config: {}", path.display());
    println!("  screen:       device={} fit={} cursor={}", display_video(list, &cfg.screen.video_device), cfg.screen.fit, cfg.screen.capture_cursor);
    println!("  camera:       enabled={} device={} {}x{} pos={} margin={},{} filter={}",
        cfg.camera.enabled, display_video(list, &cfg.camera.video_device), cfg.camera.width, cfg.camera.height,
        cfg.camera.corner, cfg.camera.margin_x, cfg.camera.margin_y, cfg.camera.filter);
    println!("  mic:          {} [{}]", display_audio(list, &cfg.audio.mic_device), cfg.audio.mic_mode);
    println!("  system audio: {}", display_audio(list, &cfg.audio.system_device));
    println!("  output:       {}x{}@{} {} {} -> {}",
        cfg.video.width, cfg.video.height,
        if cfg.video.fps_auto { "auto".to_string() } else { cfg.video.fps.to_string() },
        if cfg.video.bitrate_auto { "auto-bitrate".to_string() } else { format!("{}kbps", cfg.video.bitrate_kbps) },
        cfg.video.encoder, cfg.output.rtmp_url);
    println!("  retry:        {}",
        if cfg.output.auto_reconnect {
            match cfg.output.max_retries {
                Some(m) => format!("on, up to {m} restarts ({}s → {}s backoff)", cfg.output.retry_delay_secs, cfg.output.max_retry_delay_secs),
                None => format!("on, infinite restarts ({}s → {}s backoff)", cfg.output.retry_delay_secs, cfg.output.max_retry_delay_secs),
            }
        } else {
            "off".to_string()
        });
    if let Some(rec) = cfg.output.record_path.as_deref().filter(|p| !p.trim().is_empty()) {
        println!("  record:       {rec}");
    }
    if list.is_none() {
        println!("  (tip: `minicast devices` shows live device IDs)");
    }
}
