use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub output: OutputConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub screen: ScreenConfig,
    #[serde(default)]
    pub camera: CameraConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: OutputConfig::default(),
            video: VideoConfig::default(),
            screen: ScreenConfig::default(),
            camera: CameraConfig::default(),
            audio: AudioConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    /// RTMP URL, e.g. rtmp://a.rtmp.youtube.com/live2/KEY
    #[serde(default = "d_rtmp_placeholder")]
    pub rtmp_url: String,
    /// Optional local recording, e.g. "recording.mp4". Null/empty = disabled.
    #[serde(default)]
    pub record_path: Option<String>,
    /// Auto-restart ffmpeg when it exits with an error (network loss, frame
    /// drops, server hiccup). Applies to both `start` and `start --detach`.
    /// Default true — the stream never ends on a transient failure.
    #[serde(default = "d_true")]
    pub auto_reconnect: bool,
    /// Max restarts per `start` run. None = retry forever (default).
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Initial wait between restarts, in seconds. Doubles every attempt.
    #[serde(default = "d_retry_delay")]
    pub retry_delay_secs: u64,
    /// Upper bound for the backoff wait, in seconds.
    #[serde(default = "d_max_retry_delay")]
    pub max_retry_delay_secs: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            rtmp_url: d_rtmp_placeholder(),
            record_path: None,
            auto_reconnect: true,
            max_retries: None,
            retry_delay_secs: 2,
            max_retry_delay_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default = "d1920")]
    pub width: u32,
    #[serde(default = "d1080")]
    pub height: u32,
    /// Output frame rate. Always 60 by default, whatever the bitrate or
    /// display; ignored only while `fps_auto` is true.
    #[serde(default = "d60")]
    pub fps: u32,
    /// Follow the captured display's refresh rate (capped at AUTO_FPS_CAP,
    /// 60 — the most RTMP platforms ingest). Resolved into `fps` at start.
    /// Off by default: `set --fps auto` opts in.
    #[serde(default)]
    pub fps_auto: bool,
    /// Video bitrate in kbps. With `bitrate_auto` this is overwritten at
    /// start by what the measured uplink can carry (see `uplink`).
    #[serde(default = "d6000")]
    pub bitrate_kbps: u32,
    /// Measure upload speed at start and pick the bitrate automatically
    /// (60% of uplink, capped by a quality ceiling for the resolution+fps).
    /// Off by default: the probe hits a third-party endpoint, swings several
    /// x between runs, and under-reads good links, which starves 1080p60.
    /// `set --bitrate auto` opts in; `set --bitrate N` pins N.
    #[serde(default)]
    pub bitrate_auto: bool,
    /// videotoolbox (mac hw, default) | x264 (software fallback)
    #[serde(default = "d_videotoolbox")]
    pub encoder: String,
    #[serde(default = "d_veryfast")]
    pub preset: String,
    #[serde(default)]
    pub keyint_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    /// AVFoundation video device for screen, e.g. "4" for "Capture screen 0".
    /// Run `minicast devices` to see indices.
    #[serde(default = "d_screen")]
    pub video_device: String,
    #[serde(default = "d_true")]
    pub capture_cursor: bool,
    /// match = size the canvas to the screen's own aspect (default): nothing
    /// cropped, no black bars; 16:9 screens come out 1920x1080 as before.
    /// cover = fill a fixed canvas, crop overflow (cuts edges on 16:10).
    /// contain = fit inside canvas, letterbox/pillarbox with black bars.
    #[serde(default = "d_match")]
    pub fit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// AVFoundation video device for camera, e.g. "0".
    #[serde(default = "d_zero")]
    pub video_device: String,
    #[serde(default = "d320")]
    pub width: u32,
    #[serde(default = "d180")]
    pub height: u32,
    /// top-left | top-right | bottom-left | bottom-right | custom
    #[serde(default = "d_br")]
    pub corner: String,
    #[serde(default = "d20")]
    pub margin_x: i32,
    #[serde(default = "d20")]
    pub margin_y: i32,
    #[serde(default)]
    pub custom_x: i32,
    #[serde(default)]
    pub custom_y: i32,
    /// In-pipeline look: off | standard | studio | lowlight.
    /// Applied on the small PiP frame (cheap). macOS Control Center video
    /// effects (Portrait, Studio Light, Center Stage) still apply underneath
    /// when enabled for the terminal app — these tune the picture itself.
    #[serde(default = "d_standard")]
    pub filter: String,
    /// Capture mode to request from the camera, e.g. "1280x720". Unset =
    /// auto: the smallest landscape mode the device supports at the stream
    /// fps that still covers the PiP (see `ffmpeg::resolve_media`).
    #[serde(default)]
    pub capture_size: Option<String>,
    /// Resolved (width, height, fps) requested from the camera this run;
    /// never persisted.
    #[serde(skip)]
    pub capture_mode: Option<(u32, u32, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// AVFoundation audio device index or name substring, e.g. "3" or "MacBook Air Microphone".
    /// Empty = no mic.
    #[serde(default)]
    pub mic_device: String,
    /// Mic DSP preset: standard (transparent) | voice (noise suppression +
    /// leveling, Voice-Isolation-like) | wide (music-friendly, no suppression).
    /// macOS Control Center mic modes apply on top when enabled for the app —
    /// use one or the other to avoid double-processing.
    #[serde(default = "d_standard")]
    pub mic_mode: String,
    /// Optional second source for system audio (e.g. "BlackHole 2ch"). Empty = disabled.
    #[serde(default)]
    pub system_device: String,
    /// Mic level in dB (0 = untouched). Applied before the limiter.
    #[serde(default)]
    pub mic_gain_db: f32,
    /// System audio level in dB (0 = untouched).
    #[serde(default)]
    pub system_gain_db: f32,
    /// Lossless mic helper prepared for this run (see `mic`); never persisted.
    /// None = capture the mic through ffmpeg's own avfoundation input.
    #[serde(skip)]
    pub mic_capture: Option<crate::mic::MicCapture>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            fps_auto: false,
            bitrate_kbps: 6000,
            bitrate_auto: false,
            encoder: "videotoolbox".into(),
            preset: "veryfast".into(),
            keyint_secs: None,
        }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            video_device: "4".into(),
            capture_cursor: true,
            fit: "match".into(),
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            video_device: "0".into(),
            width: 320,
            height: 180,
            corner: "bottom-right".into(),
            margin_x: 20,
            margin_y: 20,
            custom_x: 20,
            custom_y: 20,
            filter: "standard".into(),
            capture_size: None,
            capture_mode: None,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { mic_device: String::new(), mic_mode: "standard".into(), system_device: String::new(), mic_gain_db: 0.0, system_gain_db: 0.0, mic_capture: None }
    }
}

fn d1920() -> u32 { 1920 }
fn d1080() -> u32 { 1080 }
fn d60() -> u32 { 60 }
fn d6000() -> u32 { 6000 }
fn d320() -> u32 { 320 }
fn d180() -> u32 { 180 }
fn d20() -> i32 { 20 }
fn d_videotoolbox() -> String { "videotoolbox".into() }
fn d_veryfast() -> String { "veryfast".into() }
fn d_br() -> String { "bottom-right".into() }
fn d_match() -> String { "match".into() }
fn d_standard() -> String { "standard".into() }
fn d_retry_delay() -> u64 { 2 }
fn d_max_retry_delay() -> u64 { 30 }
fn d_screen() -> String { "4".into() }
fn d_zero() -> String { "0".into() }
fn d_true() -> bool { true }
fn d_rtmp_placeholder() -> String {
    "rtmp://a.rtmp.youtube.com/live2/REPLACE-WITH-KEY".into()
}

/// Canonical config location: ~/.config/minicast/config.toml
pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".config").join("minicast").join("config.toml")
}

/// Resolve a user-supplied `--config` value, defaulting to the canonical path.
pub fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(default_config_path)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = Self::load_raw(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse without validating — used by `set` so a fresh/partial config
    /// can be edited into a valid state.
    pub fn load_raw(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    /// Load if present, otherwise create parent dirs, write defaults, and return them.
    /// Does not validate (fresh defaults have no mic yet) — `set` validates after editing.
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Self::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        Self::load_raw(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(path, text)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if !self.output.rtmp_url.starts_with("rtmp://")
            && !self.output.rtmp_url.starts_with("rtmps://")
        {
            bail!(
                "output.rtmp_url must start with rtmp:// or rtmps://, got {:?}",
                self.output.rtmp_url
            );
        }
        if self.video.width == 0 || self.video.height == 0 || self.video.fps == 0 {
            bail!("video width/height/fps must be > 0");
        }
        if self.video.fps > MAX_FPS {
            bail!("video.fps > {MAX_FPS} is not supported (got {})", self.video.fps);
        }
        match self.camera.corner.as_str() {
            "top-left" | "top-right" | "bottom-left" | "bottom-right" | "custom" => {}
            other => bail!(
                "camera.corner must be top-left|top-right|bottom-left|bottom-right|custom, got {other:?}"
            ),
        }
        if self.audio.mic_device.trim().is_empty() && self.audio.system_device.trim().is_empty() {
            bail!("audio: set at least one of mic_device or system_device (run `minicast devices`)");
        }
        match self.video.encoder.as_str() {
            "videotoolbox" | "x264" => {}
            other => bail!("video.encoder must be videotoolbox|x264, got {other:?}"),
        }
        match self.screen.fit.as_str() {
            "match" | "stretch" | "blur" | "cover" | "contain" => {}
            other => bail!("screen.fit must be match|stretch|blur|cover|contain, got {other:?}"),
        }
        match self.camera.filter.as_str() {
            "off" | "standard" | "studio" | "lowlight" => {}
            other => bail!(
                "camera.filter must be off|standard|studio|lowlight, got {other:?}"
            ),
        }
        if let Some(v) = self.camera.capture_size.as_deref() {
            if parse_size(v).is_none() {
                bail!("camera.capture_size must look like 1280x720, got {v:?}");
            }
        }
        match self.audio.mic_mode.as_str() {
            "standard" | "voice" | "wide" => {}
            other => bail!(
                "audio.mic_mode must be standard|voice|wide, got {other:?}"
            ),
        }
        for (name, db) in [("audio.mic_gain_db", self.audio.mic_gain_db), ("audio.system_gain_db", self.audio.system_gain_db)] {
            if !(-30.0..=30.0).contains(&db) {
                bail!("{name} must be between -30 and 30 dB, got {db}");
            }
        }
        if self.output.retry_delay_secs == 0 {
            bail!("output.retry_delay_secs must be >= 1 (got 0)");
        }
        if self.output.max_retry_delay_secs < self.output.retry_delay_secs {
            bail!(
                "output.max_retry_delay_secs ({}) must be >= retry_delay_secs ({})",
                self.output.max_retry_delay_secs,
                self.output.retry_delay_secs
            );
        }
        Ok(())
    }

    /// Overlay x:y in output pixels for the camera PiP.
    pub fn camera_xy(&self) -> (i32, i32) {
        let vw = self.video.width as i32;
        let vh = self.video.height as i32;
        let cw = self.camera.width as i32;
        let ch = self.camera.height as i32;
        let mx = self.camera.margin_x;
        let my = self.camera.margin_y;
        match self.camera.corner.as_str() {
            "top-left" => (mx, my),
            "top-right" => (vw - cw - mx, my),
            "bottom-left" => (mx, vh - ch - my),
            "custom" => (self.camera.custom_x, self.camera.custom_y),
            _ => (vw - cw - mx, vh - ch - my), // bottom-right default
        }
    }

}

/// Highest accepted `video.fps` (explicit values only).
pub const MAX_FPS: u32 = 120;
/// Ceiling for auto fps: the RTMP platforms cap ingest at 60 fps
/// (YouTube/Twitch/Facebook), so a 60 Hz screen streams at 60, while
/// 100/120 Hz screens also resolve to 60 — anything higher would be
/// dropped (or rejected) by the platform. Note: 60 fps needs roughly
/// double the bitrate of 30 fps at the same resolution (see `help quality`).
pub const AUTO_FPS_CAP: u32 = 60;

/// Parse "WxH" into (w, h).
pub fn parse_size(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.trim().to_lowercase().split_once('x').map(|(a, b)| (a.to_string(), b.to_string()))?;
    let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}
