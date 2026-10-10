// macOS-only streamer: AVFoundation capture + VideoToolbox (or x264 fallback) encode.
use crate::config::Config;
use crate::mic::MicCapture;
use anyhow::{Context, Result, anyhow};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Packet queue depth for raw video inputs (see the inputs comment in `build_argv`).
/// 32 frames ≈ 0.5 s at 60 fps / 1 s at 30 fps of raw Retina frames (~14 MB
/// each, ≈ 450 MB worst case). Deep enough to ride out an encode hiccup
/// without dropping (drops + CFR duplication read as 0.5x slow motion and
/// tank the actual bitrate), shallow enough to bound latency.
const VIDEO_QUEUE: &str = "32";

/// Resolve everything that depends on the actual hardware before building
/// argv: output fps (auto = captured display's refresh rate, capped at
/// AUTO_FPS_CAP) and the camera's capture mode (size + fps it really
/// supports). Falls back to the configured fps / device default if a probe
/// fails.
pub fn resolve_media(cfg: &mut Config) -> Result<()> {
    resolve_devices(cfg)?;
    if cfg.screen.fit == "match" {
        match crate::devices::screen_size(&cfg.screen.video_device) {
            Some(size) => cfg.video.width = matched_width(cfg.video.height, size),
            None => eprintln!(
                "warning: could not read the screen size, keeping {}x{} (edges may be cropped). \
                 Check screen recording permission for your terminal.",
                cfg.video.width, cfg.video.height
            ),
        }
    }
    if cfg.video.fps_auto {
        let screen_no = crate::devices::probe_avfoundation()
            .ok()
            .and_then(|l| l.video_name(&cfg.screen.video_device).map(str::to_string))
            .and_then(|n| n.trim().rsplit(' ').next().and_then(|s| s.parse::<usize>().ok()));
        let hz = screen_no
            .and_then(|n| crate::devices::display_refresh_rates().get(n).copied())
            .filter(|&hz| hz > 0);
        if let Some(hz) = hz {
            cfg.video.fps = hz.min(crate::config::AUTO_FPS_CAP);
        }
    }
    if cfg.video.bitrate_auto {
        let ceiling = crate::uplink::ceiling_kbps(cfg.video.width, cfg.video.height, cfg.video.fps);
        match crate::uplink::measure_kbps() {
            Some(up) => {
                cfg.video.bitrate_kbps = crate::uplink::pick_kbps(up, ceiling);
                eprintln!(
                    "auto-bitrate: uplink ~{:.1} Mbps -> {} kbps (ceiling {} kbps)",
                    up / 1000.0, cfg.video.bitrate_kbps, ceiling
                );
            }
            None => eprintln!(
                "auto-bitrate: upload probe failed (offline or blocked?) — using {} kbps from config. \
                 Pin one with `minicast set --bitrate N`.",
                cfg.video.bitrate_kbps
            ),
        }
    }
    if !cfg.camera.enabled {
        return Ok(());
    }
    let modes = crate::devices::camera_modes(&cfg.camera.video_device);
    cfg.camera.capture_mode = match cfg.camera.capture_size.as_deref().and_then(crate::config::parse_size) {
        Some((w, h)) => {
            let same_size: Vec<_> = modes.iter().filter(|m| (m.0, m.1) == (w, h)).cloned().collect();
            let fps = crate::devices::pick_camera_mode(&same_size, cfg.video.fps, (1, 1))
                .map_or(cfg.video.fps, |m| m.2);
            Some((w, h, fps))
        }
        None => {
            let pip = (cfg.camera.width, cfg.camera.height);
            crate::devices::pick_camera_mode(&modes, cfg.video.fps, pip)
        }
    };
    Ok(())
}

/// Canvas width for `height` that matches the screen's aspect, rounded to an
/// even number (H.264 yuv420p needs it). 3584x2016 -> 1920, 2940x1912 -> 1660.
fn matched_width(height: u32, (sw, sh): (u32, u32)) -> u32 {
    let w = (height as f64 * sw as f64 / sh as f64 / 2.0).round() as u32 * 2;
    w.max(2)
}

/// Turn the configured devices (names, or legacy numeric IDs) into the
/// CURRENT AVFoundation indices. macOS renumbers devices whenever one
/// connects or disconnects (an iPhone camera appearing shifts every screen
/// and mic), so a stored number silently points at the wrong device or at
/// nothing. Fails with the live device list instead of an opaque ffmpeg error.
fn resolve_devices(cfg: &mut Config) -> Result<()> {
    // The probe can come back empty for a moment while AVFoundation settles
    // (e.g. right after another capture process was killed): retry briefly.
    let mut list = crate::devices::probe_avfoundation()?;
    for _ in 0..3 {
        if !list.screens().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        list = crate::devices::probe_avfoundation()?;
    }
    let names = |v: Vec<(String, String)>| {
        v.iter().map(|(i, n)| format!("[{i}] {n}")).collect::<Vec<_>>().join(", ")
    };
    let hint = "Run `minicast devices`, then `minicast set --screen/--camera-device/--mic <name>`.";

    let screen = list
        .resolve_video(&cfg.screen.video_device)
        .filter(|i| list.screens().iter().any(|(s, _)| s == i))
        .ok_or_else(|| anyhow!(
            "screen {:?} is not connected. Screens now: {}. {hint}",
            cfg.screen.video_device, names(list.screens())
        ))?;
    cfg.screen.video_device = screen;

    if cfg.camera.enabled {
        cfg.camera.video_device = list
            .resolve_video(&cfg.camera.video_device)
            .filter(|i| list.cameras().iter().any(|(c, _)| c == i))
            .ok_or_else(|| anyhow!(
                "camera {:?} is not connected. Cameras now: {}. {hint}",
                cfg.camera.video_device, names(list.cameras())
            ))?;
    }
    for (label, dev) in [("mic", &mut cfg.audio.mic_device), ("system audio", &mut cfg.audio.system_device)] {
        if dev.trim().is_empty() {
            continue;
        }
        *dev = list.resolve_audio(dev).ok_or_else(|| anyhow!(
            "{label} {:?} is not connected. Audio now: {}. {hint}",
            dev, names(list.audio.clone())
        ))?;
    }
    // ffmpeg's avfoundation mic input loses ~11% of buffers; capture through
    // the helper instead, and fall back loudly if it can't be built.
    cfg.audio.mic_capture = None;
    if let Some(name) = list.audio_name(&cfg.audio.mic_device) {
        match crate::mic::MicCapture::prepare(name) {
            Ok(mic) => cfg.audio.mic_capture = Some(mic),
            Err(e) => eprintln!(
                "warning: lossless mic helper unavailable, using ffmpeg's own mic input \
                 (expect brief audio dropouts): {e:#}"
            ),
        }
    }
    Ok(())
}

/// Resolve encoder name for display. Config only allows videotoolbox|x264.
pub fn resolved_encoder(cfg: &Config) -> &str {
    cfg.video.encoder.as_str()
}

/// Re-base host-clock timestamps onto the mic helper's timeline origin.
/// With the lossless mic helper (see `mic`) ffmpeg runs with `-copyts`, so
/// avfoundation video/audio keep absolute host-clock stamps; subtracting the
/// origin puts them on the same zero as the helper's audio. `filter` is
/// `setpts` (video) or `asetpts` (audio). Empty when ffmpeg captures the mic.
fn rebase(cfg: &Config, filter: &str) -> String {
    if cfg.audio.mic_capture.is_some() {
        format!("{filter}=PTS-{}/TB,", crate::mic::ORIGIN)
    } else {
        String::new()
    }
}

/// Scale/crop chain for the screen base layer.
/// cover (default): fill the canvas exactly, crop overflow — no black bars
///   on built-in or external monitors regardless of native aspect.
/// contain: fit inside canvas, pad with black bars (classic letterbox).
fn base_video_filter(cfg: &Config, src: &str, dst: &str) -> String {
    let w = cfg.video.width;
    let h = cfg.video.height;
    let fps = cfg.video.fps;
    let rb = rebase(cfg, "setpts");
    // flags=bilinear: the fast scaler. Default bicubic on a 3584x2016 ->
    // 1920x1080 downscale every frame at 60 fps is a major CPU sink and the
    // encoder starves (dropped frames + CFR duplicates = 0.5x look).
    // Bilinear is visually identical for a live screen downscale.
    match cfg.screen.fit.as_str() {
        // Whole screen stretched to the 16:9 canvas: no bars, nothing cropped.
        // A 16:10 screen comes out ~11% wider than real (the only way to fill
        // a 16:9 player without cutting edges).
        "stretch" => format!(
            "[{src}]{rb}scale={w}:{h}:flags=bilinear,setsar=1,fps={fps}[{dst}]"
        ),
        // Whole screen, undistorted, centered on the fixed canvas; the side
        // bars are a blurred copy of the screen instead of black. The blur
        // runs on a 192x108 thumbnail, so it costs almost nothing.
        "blur" => format!(
            "[{src}]{rb}split[bg0][fg0];\
             [bg0]scale=192:108:flags=bilinear,boxblur=3:2,scale={w}:{h}:flags=bilinear,setsar=1[bg];\
             [fg0]scale={w}:{h}:flags=bilinear:force_original_aspect_ratio=decrease,setsar=1[fg];\
             [bg][fg]overlay=(W-w)/2:(H-h)/2,fps={fps}[{dst}]"
        ),
        "contain" => format!(
            "[{src}]{rb}scale={w}:{h}:flags=bilinear:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,\
             setsar=1,fps={fps}[{dst}]"
        ),
        _ => format!(
            "[{src}]{rb}scale={w}:{h}:flags=bilinear:force_original_aspect_ratio=increase,\
             crop={w}:{h},setsar=1,fps={fps}[{dst}]"
        ),
    }
}

/// Camera PiP always fills its box (cover + crop) so the overlay itself
/// never carries black bars either. `filter` adds the look preset
/// (off|standard|studio|lowlight) on the small PiP frame, where even
/// temporal denoise costs almost nothing.
fn camera_filter(cfg: &Config) -> String {
    let cw = cfg.camera.width;
    let ch = cfg.camera.height;
    let fps = cfg.video.fps;
    // fps= makes the camera constant-rate (webcams dip to 15 fps in dim
    // light) so the overlay never waits on an irregular second input.
    // flags=bilinear keeps the PiP scale cheap too.
    format!(
        "[1:v]{}scale@cam={cw}:{ch}:flags=bilinear:force_original_aspect_ratio=increase,\
         crop@cam={cw}:{ch},setsar=1,fps={fps}{}[cam]",
        rebase(cfg, "setpts"),
        camera_fx(&cfg.camera.filter)
    )
}

/// In-pipeline camera look. Applied post-scale on the tiny PiP frame, so
/// denoise/sharpen stay far away from realtime limits.
fn camera_fx(filter: &str) -> String {
    match filter {
        // Light polish: kill sensor grain, touch of color. Spatial-only
        // denoise on purpose: temporal denoise blends neighbouring frames,
        // which ghosts and smears every head movement (reads as lag).
        "standard" => ",hqdn3d=2:1.5:0:0,eq=saturation=1.08".to_string(),
        // Studio-light-like: lifted, punchy, gently sharpened.
        "studio" => {
            ",hqdn3d=2:1.5:0:0,eq=brightness=0.03:contrast=1.06:saturation=1.18,\
             unsharp=5:5:0.4"
                .to_string()
        }
        // Dim rooms: stronger denoise + exposure lift (noisy gain avoided).
        "lowlight" => ",hqdn3d=5:4:7:5,eq=brightness=0.08:contrast=1.05:saturation=1.08".to_string(),
        // "off" and anything unknown: no processing.
        _ => String::new(),
    }
}

/// Middle of the mic DSP chain for `--mic-mode` (labels added by caller).
/// All modes resample gently to 48 kHz (the native rate of Mac mics —
/// staying there avoids resample crackle), high-pass rumble/plosives, and
/// end in a limiter so peaks never clip.
///
/// - standard: transparent, nothing removed.
/// - voice: Voice-Isolation-like — FFT noise suppression + gentle leveling.
/// - wide: music-friendly — full background kept, no suppression/compression.
///
/// `gain_db` is the user level; it sits before the limiter so boosts can't clip.
fn mic_chain(mode: &str, piped: bool, gain_db: f32) -> String {
    // Soft async: only hard-stretch gaps >100 ms (dropouts), small clock
    // drift is absorbed by smooth resampling — hard stretching is what
    // clicks and crackles. The helper's pipe is already gap-free 48 kHz on
    // the shared timeline, so there it must not be re-based (first_pts) or
    // stuffed with silence (async).
    let src = if piped {
        "aresample=48000"
    } else {
        "aresample=48000:async=1:min_hard_comp=0.100:first_pts=0"
    };
    let out = format!(
        "{}aformat=sample_rates=48000:channel_layouts=stereo,alimiter=limit=0.95:level=disabled",
        gain_filter("mic", gain_db)
    );
    match mode {
        "voice" => format!(
            "{src},highpass=f=80,afftdn=nf=-25:nr=12,\
             acompressor=threshold=-18dB:ratio=3:attack=20:release=200:makeup=6dB,{out}"
        ),
        "wide" => format!("{src},highpass=f=60,{out}"),
        _ => format!("{src},highpass=f=80,{out}"),
    }
}

/// Named `volume@<name>` filter with a trailing comma. It is always present,
/// even at 0 dB, so a live command can change the level without a restart.
fn gain_filter(name: &str, db: f32) -> String {
    format!("volume@{name}={db}dB,")
}

/// Transparent chain for system audio (never denoise music/desktop sound).
fn system_chain(cfg: &Config) -> String {
    let gain = gain_filter("sys", cfg.audio.system_gain_db);
    // Next to the mic helper, keep the device's host-clock stamps (re-based
    // onto the helper's origin) instead of resetting them to zero.
    if cfg.audio.mic_capture.is_some() {
        format!(
            "{}aresample=48000,{gain}aformat=sample_rates=48000:channel_layouts=stereo",
            rebase(cfg, "asetpts")
        )
    } else {
        format!(
            "aresample=48000:async=1:min_hard_comp=0.100:first_pts=0,{gain}\
             aformat=sample_rates=48000:channel_layouts=stereo"
        )
    }
}

/// Input args for the mic: the helper's raw PCM pipe (gap-free 48 kHz mono
/// float, see `mic`) or ffmpeg's own lossy avfoundation capture.
fn mic_input(cfg: &Config) -> Vec<String> {
    let args: &[&str] = if cfg.audio.mic_capture.is_some() {
        &["-thread_queue_size", "1024", "-f", "f32le", "-ar", "48000", "-ac", "1", "-i", "pipe:0"]
    } else {
        &["-thread_queue_size", "1024", "-f", "avfoundation", "-i"]
    };
    let mut v: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if cfg.audio.mic_capture.is_none() {
        v.push(format!(":{}", cfg.audio.mic_device));
    }
    v
}

/// Push the screen (0:v) and optional camera (1:v) inputs and return the
/// filtergraph that composes them into `[video]`. Shared by the stream and
/// the GUI preview so both show exactly the same picture.
fn video_inputs(cfg: &Config, fps: u32, args: &mut Vec<String>) -> String {
    let (cam_x, cam_y) = cfg.camera_xy();
    let cursor = if cfg.screen.capture_cursor { "1" } else { "0" };
    // Video queues are sized to ride out encode hiccups: a raw Retina frame
    // is ~14 MB, so VIDEO_QUEUE=32 bounds worst-case RAM (~450 MB) while
    // holding ~0.5-1 s. Shallower (8) drops on any hiccup; with CFR output
    // each drop is a duplicated frame, which viewers see as half-speed motion
    // and which compresses to nothing, so YouTube reports a very low bitrate
    // even on excellent upload.
    args.extend([
        "-thread_queue_size".into(),
        VIDEO_QUEUE.into(),
        "-f".into(),
        "avfoundation".into(),
        "-capture_cursor".into(),
        cursor.into(),
        // Native uyvy422 avoids a pixel-format override. -framerate IS
        // honored (measured: 30/60 -> ~30/58 fps) despite the harmless
        // "Configuration of video device failed" warning; without it the
        // device defaults to 30.
        "-pixel_format".into(),
        "uyvy422".into(),
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        format!("{}:", cfg.screen.video_device),
    ]);

    if !cfg.camera.enabled {
        return base_video_filter(cfg, "0:v", "video");
    }
    args.extend([
        "-thread_queue_size".into(),
        VIDEO_QUEUE.into(),
        "-f".into(),
        "avfoundation".into(),
        // Request an explicit mode: without it macOS picks its own
        // default (e.g. 1080x1920 portrait, unclocked), heavy and choppy.
        // The mode comes from the device itself (or camera.capture_size).
        "-pixel_format".into(),
        "uyvy422".into(),
    ]);
    // Camera rate: what the device actually offers (<= stream fps); the
    // camera filter chain then duplicates up to the stream fps.
    let mut cam_fps = fps;
    if let Some((mw, mh, cf)) = cfg.camera.capture_mode {
        args.extend(["-video_size".into(), format!("{mw}x{mh}")]);
        cam_fps = cf;
    }
    args.extend([
        "-framerate".into(),
        cam_fps.to_string(),
        "-i".into(),
        format!("{}:", cfg.camera.video_device),
    ]);
    format!(
        "{};{};[base][cam]overlay@cam={cam_x}:{cam_y}:format=yuv420[video]",
        base_video_filter(cfg, "0:v", "base"),
        camera_filter(cfg)
    )
}

/// Build the ffmpeg argv (without the leading "ffmpeg").
pub fn build_argv(cfg: &Config) -> Result<Vec<String>> {
    let w = cfg.video.width;
    let h = cfg.video.height;
    let fps = cfg.video.fps;
    let br = format!("{}k", cfg.video.bitrate_kbps);
    // 1 s VBV window: tight enough for live CBR behaviour (stable bitrate
    // YouTube actually receives), loose enough to absorb a scene cut.
    let buf = br.clone();
    let keyint = cfg.video.keyint_secs.unwrap_or(2) * fps;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "info".into(),
        // Progress line every 5s (fps, dup, drop, speed, bitrate) in the log,
        // so `minicast stream logs` shows exactly where frames are lost.
        "-stats".into(),
        "-stats_period".into(),
        "5".into(),
        // Resilience + low-latency input tuning:
        // genpts = rebuild timestamps across gaps so frame drops don't desync
        // A/V; discardcorrupt = drop corrupt packets instead of aborting.
        // (Deliberately NO +nobuffer and NO -use_wallclock_as_timestamps:
        // nobuffer starves the encoder on hiccups, and wallclock replaces
        // the device clock with epoch-based offsets that skew inputs apart
        // by hundreds of ms — the classic 0.5x slow-motion + low-bitrate
        // symptom. Device timestamps + genpts stay in sync.)
        "-fflags".into(),
        "+genpts+discardcorrupt".into(),
        "-err_detect".into(),
        "ignore_err".into(),
        "-flags".into(),
        "low_delay".into(),
        // Large enough to actually estimate the AVFoundation rate. 32 bytes
        // can't cover one Retina frame, so ffmpeg guesses 1000k tbr and the
        // fps filter judders trying to convert from it ("not enough frames
        // to estimate rate"). 5 MB / 1 s costs ~1 s of startup, then the
        // rate is correct for the whole stream.
        "-probesize".into(),
        "5M".into(),
        "-analyzeduration".into(),
        "1000000".into(),
    ];
    let piped_mic = cfg.audio.mic_capture.is_some() && !cfg.audio.mic_device.trim().is_empty();
    if piped_mic {
        // stdin carries the mic PCM (no keyboard commands). -copyts keeps the
        // avfoundation host-clock stamps so video lines up with the helper's
        // audio timeline (see `rebase`). NOT -shortest: it stalls the muxer.
        args.extend(["-nostdin".into(), "-copyts".into()]);
    }

    // --- inputs ---
    // Video queues are sized to ride out encode hiccups: a raw Retina frame
    // is ~14 MB, so VIDEO_QUEUE=32 bounds worst-case RAM (~450 MB) while
    // holding ~0.5-1 s. Shallower (8) drops on any hiccup; with CFR output
    // each drop is a duplicated frame — viewers see half-speed motion and
    // duplicates compress to nothing, so YouTube reports a very low bitrate
    // even on excellent upload. Audio packets are tiny, so those stay deep.
    let video_label = "[video]".to_string();
    let filter = video_inputs(cfg, fps, &mut args);

    // Audio inputs: mic and optional system mix, all at 48 kHz end to end.
    // macOS mics natively deliver 48 kHz — converting to 44.1 kHz and back
    // is a classic crackle source, so the whole chain stays at 48 kHz
    // (AAC 48 kHz is the video standard; YouTube/Twitch accept it).
    let has_mic = !cfg.audio.mic_device.trim().is_empty();
    let has_sys = !cfg.audio.system_device.trim().is_empty();
    // Audio filters live in their OWN filtergraph. Sharing one graph with the
    // video makes ffmpeg's scheduler stall video whenever audio waits on a
    // gap (measured: 225 of 901 real screen frames reached the encoder, the
    // rest duplicated = visible lag; separate graphs: 898 of 901).
    let mut afilter = String::new();
    let audio_label: String;
    if has_mic && has_sys {
        let mic_idx = if cfg.camera.enabled { 2 } else { 1 };
        let sys_idx = mic_idx + 1;
        args.extend(mic_input(cfg));
        args.extend(["-thread_queue_size".into(), "1024".into(), "-f".into(), "avfoundation".into(), "-i".into(), format!(":{}", cfg.audio.system_device)]);
        // Denoise/level the mic only — never the system mix — then mix with
        // normalize=0 and a final limiter so stacked sources can't clip.
        afilter.push_str(&format!(
            "[{mic_idx}:a]{mchain}[m0];[{sys_idx}:a]{schain}[s0];\
             [m0][s0]amix=inputs=2:duration=longest:dropout_transition=0:normalize=0[mix];\
             [mix]alimiter=limit=0.95:level=disabled[audio]",
            mchain = mic_chain(&cfg.audio.mic_mode, piped_mic, cfg.audio.mic_gain_db),
            schain = system_chain(cfg),
        ));
        audio_label = "[audio]".to_string();
    } else if has_mic || has_sys {
        let chain = if has_mic {
            mic_chain(&cfg.audio.mic_mode, piped_mic, cfg.audio.mic_gain_db)
        } else {
            system_chain(cfg)
        };
        let idx = if cfg.camera.enabled { 2 } else { 1 };
        if has_mic {
            args.extend(mic_input(cfg));
        } else {
            args.extend(["-thread_queue_size".into(), "1024".into(), "-f".into(), "avfoundation".into(), "-i".into(), format!(":{}", cfg.audio.system_device)]);
        }
        audio_label = format!("{idx}:a");
        afilter.push_str(&format!("[{idx}:a]{chain}[audio]"));
    } else {
        // unreachable: validated earlier, but keep ffmpeg-safe silence fallback
        args.extend([
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "anullsrc=r=48000:cl=stereo".into(),
        ]);
        let idx = if cfg.camera.enabled { 2 } else { 1 };
        audio_label = format!("{idx}:a");
    }
    // If single-source path already created [audio], use it; else map computed label.
    let final_audio = if audio_label == "[audio]" { "[audio]".to_string() } else if afilter.contains("[audio]") {
        "[audio]".to_string()
    } else {
        audio_label
    };

    args.extend(["-filter_complex".into(), filter]);
    if !afilter.is_empty() {
        args.extend(["-filter_complex".into(), afilter]);
    }
    args.extend(["-map".into(), video_label, "-map".into(), final_audio]);

    // --- output resilience (survive lag, jitter, short outages) ---
    // max_muxing_queue_size: absorb bursts instead of aborting with
    // "Too many packets buffered" when the network stalls.
    // fps_mode=cfr: constant frame rate output even when capture drops frames.
    // (The network-side resilience lives in the fifo muxer below.)
    args.extend([
        "-max_muxing_queue_size".into(),
        "2048".into(),
        "-fps_mode".into(),
        "cfr".into(),
    ]);

    // --- video encode (single encode shared by stream; record re-encodes small CRF copy) ---
    // Full quality: exact user bitrate as both target and ceiling, 2s keyframes
    // (YouTube/Twitch requirement), high profile, yuv420p for max compatibility.
    // Framerate lives ONLY in the fps filters + fps_mode=cfr: a second `-r`
    // conversion after the filter duplicates/drops again and reads as judder.
    // CBR enforcement: without it both encoders dip far below the target on
    // static screen content (duplicated frames compress to nothing), so the
    // platform reports a very low bitrate even on excellent upload.
    match resolved_encoder(cfg) {
        "x264" => {
            args.extend([
                "-c:v".into(), "libx264".into(),
                "-preset".into(), cfg.video.preset.clone(),
                "-tune".into(), "zerolatency".into(),
                "-b:v".into(), br.clone(),
                "-maxrate".into(), br.clone(),
                "-bufsize".into(), buf,
                "-nal-hrd".into(), "cbr".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-g".into(), keyint.to_string(),
                "-keyint_min".into(), keyint.to_string(),
            ]);
        }
        _ => {
            args.extend([
                "-c:v".into(), "h264_videotoolbox".into(),
                "-b:v".into(), br.clone(),
                "-maxrate".into(), br.clone(),
                "-bufsize".into(), buf,
                "-g".into(), keyint.to_string(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-realtime".into(), "true".into(),
                // Constant bit rate (macOS 13+): hold the target instead of
                // dipping on static content, so the received bitrate matches
                // the configured one.
                "-constant_bit_rate".into(), "true".into(),
                // Fall back to software encode rather than failing when the
                // hardware encoder is busy — the stream stays up.
                "-allow_sw".into(), "1".into(),
                "-profile:v".into(), "high".into(),
            ]);
        }
    }
    args.extend([
        "-c:a".into(), "aac".into(),
        "-b:a".into(), "160k".into(),
        "-ar".into(), "48000".into(),
        "-ac".into(), "2".into(),
    ]);

    // --- output: network isolated from the encoder ---
    // A plain `-f flv rtmp://…` writes synchronously, so one slow upload
    // stalls capture + encode. The fifo muxer adds a SHORT queue (~1 s) and
    // reconnects in place after a dropped connection (waiting for a
    // keyframe) without restarting capture. drop_pkts_on_overflow is OFF:
    // when full, fifo would flush the ENTIRE queue (seconds of video,
    // keyframes included), which viewers see as freezes and jumps. Blocking
    // instead pushes back to the shallow capture queues, which drop single
    // frames — graceful slowdown rather than a hole in the stream.
    // max_recovery_attempts: after ~10 failed re-opens (about 10-20 s) the fifo
    // gives up, ffmpeg exits non-zero and the supervisor restarts everything.
    // Unlimited (the default) can wedge in "Recovery failed: Invalid argument"
    // forever while the process stays up, so nothing ever restarts it.
    // no_duration_filesize keeps FLV metadata sane across reconnects.
    // With a record path, one tee muxer feeds both the fifo'd RTMP output
    // and a fragmented MP4 (playable even if the process is killed) from
    // the SAME encode — no second encode of the raw screen.
    let fifo = "queue_size=150:drop_pkts_on_overflow=0:attempt_recovery=1:\
                max_recovery_attempts=10:recover_any_error=1:recovery_wait_time=1:restart_with_keyframe=1";
    match cfg.output.record_path.as_ref().filter(|p| !p.trim().is_empty()) {
        Some(path) => args.extend([
            // tee can't tell each slave's needs; MP4 requires global headers.
            "-flags:v".into(),
            "+global_header".into(),
            "-flags:a".into(),
            "+global_header".into(),
            "-f".into(),
            "tee".into(),
            format!(
                "[f=fifo:onfail=ignore:fifo_format=flv:{fifo}:format_opts=flvflags\\=no_duration_filesize]{}\
                 |[f=mp4:movflags=+frag_keyframe+empty_moov]{path}",
                cfg.output.rtmp_url
            ),
        ]),
        None => args.extend([
            "-f".into(),
            "fifo".into(),
            "-fifo_format".into(),
            "flv".into(),
            "-format_opts".into(),
            "flvflags=no_duration_filesize".into(),
            "-queue_size".into(),
            "150".into(),
            "-drop_pkts_on_overflow".into(),
            "0".into(),
            "-attempt_recovery".into(),
            "1".into(),
            "-max_recovery_attempts".into(),
            "10".into(),
            "-recover_any_error".into(),
            "1".into(),
            "-recovery_wait_time".into(),
            "1".into(),
            "-restart_with_keyframe".into(),
            "1".into(),
            cfg.output.rtmp_url.clone(),
        ]),
    }

    // Silence unused-variable warnings if canvas vars only used in filter strings.
    let _ = (w, h);
    Ok(args)
}

/// Set by Ctrl-C / SIGTERM. The CLI must outlive ffmpeg: if it exits first,
/// ffmpeg is orphaned and keeps streaming (and holding the camera) unseen.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// How long ffmpeg gets to finish after SIGINT before it is killed. A wedged
/// output (dead network) can block its graceful shutdown indefinitely.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Turn Ctrl-C / SIGTERM into a flag that `run_once` acts on, instead of
/// letting the default handler kill the CLI before ffmpeg has stopped.
pub fn install_interrupt_handler() -> Result<()> {
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst)).context("failed to install the Ctrl-C handler")
}

/// Run ffmpeg once and wait for it. With the lossless mic helper, start the
/// helper on a fresh timeline origin and feed its PCM to ffmpeg's stdin. If
/// the helper dies first, stop ffmpeg and report failure so the caller's
/// retry loop restarts both (ffmpeg alone would carry on with no audio).
pub fn run_once(argv: &[String], mic: Option<&MicCapture>) -> Result<ExitStatus> {
    let Some(mic) = mic else {
        eprintln!("$ ffmpeg {}", shell_join(argv));
        let mut ffmpeg = Command::new("ffmpeg").args(argv).spawn().context("failed to start ffmpeg (is it installed?)")?;
        return wait_for(&mut ffmpeg, None, interrupted, SHUTDOWN_GRACE);
    };
    let origin = mic.now()?;
    let argv: Vec<String> = argv.iter().map(|a| a.replace(crate::mic::ORIGIN, &origin)).collect();
    eprintln!("$ {} {:?} {origin} | ffmpeg {}", mic.exe.display(), mic.device, shell_join(&argv));
    let mut helper = mic.capture_command(&origin).stdout(Stdio::piped()).spawn().context("failed to start the mic helper")?;
    let pcm = helper.stdout.take().ok_or_else(|| anyhow!("mic helper started without a stdout pipe"))?;
    let mut ffmpeg = match Command::new("ffmpeg").args(&argv).stdin(Stdio::from(pcm)).spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = helper.kill();
            let _ = helper.wait();
            return Err(anyhow!(e).context("failed to start ffmpeg (is it installed?)"));
        }
    };
    let status = wait_for(&mut ffmpeg, Some(&mut helper), interrupted, SHUTDOWN_GRACE);
    let _ = helper.kill();
    let _ = helper.wait();
    status
}

/// Wait for `ffmpeg`. When `stop()` turns true, ask it to finish with SIGINT
/// and kill it after `grace`. When the mic helper dies first, kill ffmpeg and
/// return exit code 1 so the caller restarts both.
fn wait_for(
    ffmpeg: &mut Child,
    mut helper: Option<&mut Child>,
    stop: impl Fn() -> bool,
    grace: Duration,
) -> Result<ExitStatus> {
    let mut asked_at: Option<Instant> = None;
    loop {
        if let Some(status) = ffmpeg.try_wait()? {
            return Ok(status);
        }
        if !stop() {
            if let Some(status) = helper.as_mut().map(|h| h.try_wait()).transpose()?.flatten() {
                eprintln!("mic helper exited ({status}); stopping ffmpeg so both restart together.");
                let _ = ffmpeg.kill();
                let _ = ffmpeg.wait();
                return Ok(ExitStatus::from_raw(1 << 8));
            }
        } else {
            match asked_at {
                None => {
                    let _ = Command::new("kill").args(["-INT", &ffmpeg.id().to_string()]).status();
                    asked_at = Some(Instant::now());
                }
                Some(t) if t.elapsed() >= grace => {
                    eprintln!("ffmpeg did not stop within {}s (its output is probably stuck); killing it.", grace.as_secs());
                    let _ = ffmpeg.kill();
                }
                Some(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Hide the stream key (last path segment) of every rtmp(s):// URL in `arg`,
/// including URLs embedded in a tee spec. The echo of the command is shown on
/// screen, which may be the screen being streamed.
fn mask_stream_keys(arg: &str) -> String {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(start) = rest.find("rtmp") {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        if tail.starts_with("rtmp://") || tail.starts_with("rtmps://") {
            let end = tail.find(['|', ']', ' ']).unwrap_or(tail.len());
            let (url, after) = tail.split_at(end);
            out.push_str(&crate::streams::mask_rtmp_url(url));
            rest = after;
        } else {
            out.push_str("rtmp");
            rest = &tail["rtmp".len()..];
        }
    }
    out.push_str(rest);
    out
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| mask_stream_keys(a))
        .map(|a| if a.chars().any(|c| c.is_whitespace() || c == ';' || c == '[' || c == ']') {
            format!("'{a}'")
        } else {
            a.clone()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.output.rtmp_url = "rtmp://live.example/app/key".into();
        cfg.audio.mic_device = "3".into();
        cfg
    }

    #[test]
    fn cover_is_default_and_has_no_pad() {
        let argv = build_argv(&test_cfg()).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("force_original_aspect_ratio=increase"), "{joined}");
        assert!(joined.contains("crop=1920:1080"), "{joined}");
        assert!(joined.contains("overlay@cam="), "expected camera overlay: {joined}");
        assert!(joined.contains("-capture_cursor"), "{joined}");
    }

    #[test]
    fn match_fit_follows_the_screen_aspect() {
        assert_eq!(matched_width(1080, (3584, 2016)), 1920); // 16:9 monitor unchanged
        assert_eq!(matched_width(1080, (2560, 1664)), 1662); // 16:10 MacBook
        assert_eq!(matched_width(1080, (2940, 1912)) % 2, 0);
    }

    #[test]
    fn echoed_command_hides_the_stream_key() {
        let plain = vec!["-f".to_string(), "fifo".to_string(), "rtmp://a.rtmp.youtube.com/live2/SECRET-KEY".to_string()];
        let echo = shell_join(&plain);
        assert!(!echo.contains("SECRET-KEY") && echo.contains("rtmp://a.rtmp.youtube.com/live2/****"), "{echo}");
        let tee = vec!["[f=fifo:x=1]rtmps://live.example/app/SECRET-KEY|[f=mp4]out.mp4".to_string()];
        let echo = shell_join(&tee);
        assert!(!echo.contains("SECRET-KEY") && echo.contains("|[f=mp4]out.mp4"), "{echo}");
    }

    #[test]
    fn stretch_fit_fills_the_canvas_without_crop_or_pad() {
        let mut cfg = test_cfg();
        cfg.screen.fit = "stretch".into();
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("scale=1920:1080:flags=bilinear,setsar=1"), "{joined}");
        assert!(!joined.contains("crop=1920:1080") && !joined.contains("pad="), "{joined}");
    }

    #[test]
    fn blur_fit_keeps_the_full_screen_on_a_fixed_canvas() {
        let mut cfg = test_cfg();
        cfg.screen.fit = "blur".into();
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("boxblur") && joined.contains("overlay=(W-w)/2:(H-h)/2"), "{joined}");
        assert!(joined.contains("force_original_aspect_ratio=decrease"), "{joined}");
        assert!(!joined.contains("crop=1920:1080"), "{joined}");
    }

    #[test]
    fn contain_keeps_letterbox() {
        let mut cfg = test_cfg();
        cfg.screen.fit = "contain".into();
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("force_original_aspect_ratio=decrease"), "{joined}");
        assert!(joined.contains("pad=1920:1080"), "{joined}");
    }

    #[test]
    fn camera_disabled_has_no_overlay() {
        let mut cfg = test_cfg();
        cfg.camera.enabled = false;
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(!joined.contains("overlay="), "{joined}");
        assert!(joined.contains("crop=1920:1080"), "{joined}");
    }

    #[test]
    fn output_survives_drops_and_reconnects_cleanly() {
        let joined = build_argv(&test_cfg()).unwrap().join(" ");
        // Burst absorption + constant frame rate + drop-instead-of-die.
        assert!(joined.contains("-max_muxing_queue_size 2048"), "{joined}");
        assert!(joined.contains("fps_mode cfr"), "{joined}");
        // Network isolated from encode via the fifo muxer (these options are
        // silently ignored by `-f flv`, so the muxer itself must be fifo).
        assert!(joined.contains("-f fifo -fifo_format flv"), "{joined}");
        assert!(joined.contains("-drop_pkts_on_overflow 0"), "{joined}");
        assert!(joined.contains("-max_recovery_attempts 10"), "{joined}");
        assert!(joined.contains("-attempt_recovery 1"), "{joined}");
        assert!(joined.contains("-restart_with_keyframe 1"), "{joined}");
        // Timestamp repair + corrupt-packet tolerance on inputs (no nobuffer,
        // no wallclock — those caused the 0.5x slow-motion + low-bitrate bug).
        assert!(joined.contains("+genpts+discardcorrupt"), "{joined}");
        assert!(!joined.contains("nobuffer"), "{joined}");
        assert!(!joined.contains("wallclock"), "{joined}");
        // Probesize large enough to estimate the real AVFoundation rate.
        assert!(joined.contains("-probesize 5M"), "{joined}");
        assert!(joined.contains("-analyzeduration 1000000"), "{joined}");
        // Video queues ride out hiccups without dropping, deep audio queue.
        assert_eq!(joined.matches("-thread_queue_size 32 ").count(), 2, "{joined}");
        assert_eq!(joined.matches("-thread_queue_size 1024 ").count(), 1, "{joined}");
        // Audio stays at macOS-native 48 kHz with soft gap compensation.
        assert!(joined.contains("aresample=48000:async=1:min_hard_comp=0.100:first_pts=0"), "{joined}");
        assert!(joined.contains("aformat=sample_rates=48000"), "{joined}");
        assert!(joined.contains("-ar 48000"), "{joined}");
        // Framerate lives in the fps filters + fps_mode only — no second `-r`
        // conversion after the filter (that judders).
        assert!(!joined.contains(" -r "), "{joined}");
        assert!(joined.contains("fps=60"), "{joined}");
        // Fast scaler keeps the 4K->1080p downscale off the critical path.
        assert!(joined.contains("flags=bilinear"), "{joined}");
        // Healthy FLV metadata across reconnects.
        assert!(joined.contains("no_duration_filesize"), "{joined}");
        // Full quality: 2s keyframes, high profile, yuv420p.
        assert!(joined.contains("-g 120"), "{joined}");
        assert!(joined.contains("high"), "{joined}");
        assert!(joined.contains("yuv420p"), "{joined}");
        // CBR hold: the received bitrate matches the configured one.
        assert!(joined.contains("-constant_bit_rate true"), "{joined}");
    }

    #[test]
    fn audio_filters_never_share_the_video_graph() {
        // A shared graph stalls video on audio gaps (15 fps real frames).
        for system_device in ["", "1"] {
            let mut cfg = test_cfg();
            cfg.audio.system_device = system_device.into();
            let argv = build_argv(&cfg).unwrap();
            let graphs: Vec<&String> = argv
                .windows(2)
                .filter(|w| w[0] == "-filter_complex")
                .map(|w| &w[1])
                .collect();
            assert_eq!(graphs.len(), 2, "{argv:?}");
            assert!(graphs[0].contains("[video]") && !graphs[0].contains("[audio]"), "{}", graphs[0]);
            assert!(graphs[1].contains("[audio]") && !graphs[1].contains("[video]"), "{}", graphs[1]);
        }
    }

    #[test]
    fn helper_mic_pipes_pcm_and_shares_the_host_clock_with_video() {
        let mut cfg = test_cfg();
        cfg.audio.mic_capture = Some(crate::mic::MicCapture { exe: "/x/miccap".into(), device: "Mic".into() });
        let argv = build_argv(&cfg).unwrap();
        let joined = argv.join(" ");
        // Raw PCM on stdin, no keyboard reads, host-clock stamps kept.
        assert!(joined.contains("-f f32le -ar 48000 -ac 1 -i pipe:0"), "{joined}");
        assert!(joined.contains("-nostdin -copyts"), "{joined}");
        // -shortest stalls the muxer; helper death is handled by the launcher.
        assert!(!joined.contains("-shortest"), "{joined}");
        // Video is re-based onto the helper's origin; audio is not re-stamped.
        assert!(joined.contains("[0:v]setpts=PTS-@MIC_ORIGIN@/TB,scale="), "{joined}");
        assert!(joined.contains("[2:a]aresample=48000,highpass"), "{joined}");
        // The camera shares the host clock, so it is re-based the same way.
        assert!(joined.contains("[1:v]setpts=PTS-@MIC_ORIGIN@/TB,scale@cam=320:180"), "{joined}");
        assert!(!joined.contains("first_pts"), "{joined}");
        assert!(!joined.contains("-f avfoundation -i :"), "{joined}");
    }

    #[test]
    fn x264_uses_cbr_without_second_r_conversion() {
        let mut cfg = test_cfg();
        cfg.video.encoder = "x264".into();
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("-nal-hrd cbr"), "{joined}");
        assert!(!joined.contains(" -r "), "{joined}");
        assert!(joined.contains("-tune zerolatency"), "{joined}");
    }

    #[test]
    fn record_shares_the_single_encode_via_tee() {
        let mut cfg = test_cfg();
        cfg.output.record_path = Some("out.mp4".into());
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("-f tee [f=fifo:onfail=ignore:fifo_format=flv:"), "{joined}");
        assert!(joined.contains("rtmp://live.example/app/key|[f=mp4:"), "{joined}");
        assert!(joined.contains("]out.mp4"), "{joined}");
        // One video encoder only — no second encode of the raw screen.
        assert_eq!(joined.matches("-c:v").count(), 1, "{joined}");
    }

    #[test]
    fn videotoolbox_falls_back_to_software_instead_of_dying() {
        let joined = build_argv(&test_cfg()).unwrap().join(" ");
        assert!(joined.contains("-allow_sw 1"), "{joined}");
    }

    #[test]
    fn a_stuck_ffmpeg_is_killed_after_the_grace_period() {
        // Ignores SIGINT like an ffmpeg wedged on a dead network write.
        let mut child = Command::new("sh").args(["-c", "trap '' INT; sleep 30"]).spawn().unwrap();
        let started = Instant::now();
        let status = wait_for(&mut child, None, || true, Duration::from_millis(300)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(status.code(), None, "expected death by signal, got {status}");
    }

    #[test]
    fn a_cooperative_ffmpeg_stops_on_sigint_without_being_killed() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let status = wait_for(&mut child, None, || true, Duration::from_secs(30)).unwrap();
        assert_eq!(status.signal(), Some(2), "{status}");
    }

    #[test]
    fn gain_is_named_and_applied_before_the_limiter() {
        let flat = build_argv(&test_cfg()).unwrap().join(" ");
        assert!(flat.contains("volume@mic=0dB,"), "named so a live command can change it: {flat}");

        let mut cfg = test_cfg();
        cfg.audio.mic_gain_db = 6.0;
        cfg.audio.system_device = "1".into();
        cfg.audio.system_gain_db = -4.5;
        let joined = build_argv(&cfg).unwrap().join(" ");
        assert!(joined.contains("volume@mic=6dB,aformat=sample_rates=48000:channel_layouts=stereo,alimiter"), "{joined}");
        assert!(joined.contains("volume@sys=-4.5dB,aformat"), "{joined}");
    }

    #[test]
    fn mic_modes_shape_the_dsp_chain() {
        // standard (default): transparent — highpass + limiter, no suppression.
        let std = build_argv(&test_cfg()).unwrap().join(" ");
        assert!(std.contains("highpass=f=80"), "{std}");
        assert!(std.contains("alimiter=limit=0.95"), "{std}");
        assert!(!std.contains("afftdn"), "{std}");
        assert!(!std.contains("acompressor"), "{std}");

        // voice: suppression + leveling.
        let mut cfg = test_cfg();
        cfg.audio.mic_mode = "voice".into();
        let voice = build_argv(&cfg).unwrap().join(" ");
        assert!(voice.contains("afftdn=nf=-25:nr=12"), "{voice}");
        assert!(voice.contains("acompressor=threshold=-18dB"), "{voice}");

        // wide: music kept — lower highpass, no suppression or compression.
        cfg.audio.mic_mode = "wide".into();
        let wide = build_argv(&cfg).unwrap().join(" ");
        assert!(wide.contains("highpass=f=60"), "{wide}");
        assert!(!wide.contains("afftdn"), "{wide}");
        assert!(!wide.contains("acompressor"), "{wide}");

        // Dual-source: mic gets its chain, system mix stays clean, mix limited.
        cfg.audio.mic_mode = "voice".into();
        cfg.audio.system_device = "1".into();
        let dual = build_argv(&cfg).unwrap().join(" ");
        assert!(dual.contains("normalize=0"), "{dual}");
        assert!(dual.contains("[mix]alimiter=limit=0.95"), "{dual}");
    }

    #[test]
    fn camera_filters_render_on_the_pip() {
        // standard default: light denoise + touch of color.
        let std = build_argv(&test_cfg()).unwrap().join(" ");
        assert!(std.contains("hqdn3d=2:1.5:0:0"), "{std}");

        let mut cfg = test_cfg();
        cfg.camera.filter = "off".into();
        let off = build_argv(&cfg).unwrap().join(" ");
        assert!(!off.contains("hqdn3d") && !off.contains("unsharp"), "{off}");

        cfg.camera.filter = "studio".into();
        let studio = build_argv(&cfg).unwrap().join(" ");
        assert!(studio.contains("unsharp=5:5:0.4"), "{studio}");

        cfg.camera.filter = "lowlight".into();
        let low = build_argv(&cfg).unwrap().join(" ");
        assert!(low.contains("hqdn3d=5:4:7:5"), "{low}");
        assert!(low.contains("brightness=0.08"), "{low}");
    }
}
