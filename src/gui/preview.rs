//! Live preview feeds. The screen comes from ScreenCaptureKit through GPUI:
//! native resolution, full frame rate, and GPU frames painted without a copy.
//! The camera and mic come from small ffmpeg processes. Layout (fit, camera
//! position and size) is drawn by the window, so changing it never restarts a
//! capture; only changing a device does.
use crate::config::Config;
use super::convert::{Converted, Converter};
use crate::devices;
use core_foundation::base::TCFType;
use core_video::pixel_buffer::CVPixelBuffer;
use gpui_kit::*;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Camera preview frame (the window crops it to the box's shape).
const CAMERA_SIZE: (u32, u32) = (640, 360);
const CAMERA_FPS: u32 = 30;
/// Mic meter window: 50 ms of 16 kHz mono f32.
const METER_SAMPLES: usize = 800;

#[derive(Clone, Debug)]
pub enum Feed {
    Off,
    Starting,
    Running,
    Failed(String),
}

/// A CoreVideo buffer handed from the capture thread to the UI thread.
struct Frame(Converted);

// SAFETY: CoreVideo pixel buffers are reference counted and documented as
// safe to retain, release and read from any thread. We only retain, release
// and hand the buffer to the GPU.
unsafe impl Send for Frame {}

struct ScreenFeed {
    _stream: Box<dyn ScreenCaptureStream>,
    latest: Arc<Mutex<Option<Frame>>>,
}

/// A background ffmpeg feed that can be restarted or stopped at any time.
struct Worker {
    /// Bumped on every start/stop; a thread exits when it no longer matches.
    generation: AtomicU64,
    child: Mutex<Option<Child>>,
    state: Mutex<Feed>,
    frame: Mutex<Option<Vec<u8>>>,
    frame_seq: AtomicU64,
}

impl Worker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(0),
            child: Mutex::new(None),
            state: Mutex::new(Feed::Off),
            frame: Mutex::new(None),
            frame_seq: AtomicU64::new(0),
        })
    }

    /// Stop the current run and return the generation for the next one.
    fn restart(&self) -> u64 {
        let generation = self.stop();
        *lock(&self.state) = Feed::Starting;
        generation
    }

    fn stop(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *lock(&self.state) = Feed::Off;
        generation
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// Take ownership of a freshly spawned child, unless a newer run won.
    fn adopt(&self, mut child: Child, generation: u64) -> bool {
        let mut slot = lock(&self.child);
        if !self.current(generation) {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        *slot = Some(child);
        true
    }

    fn fail(&self, generation: u64, message: String) {
        if self.current(generation) {
            *lock(&self.state) = Feed::Failed(message);
        }
    }
}

pub struct Preview {
    screen: Option<ScreenFeed>,
    screen_state: Feed,
    screen_task: Option<Task<()>>,
    camera: Arc<Worker>,
    camera_image: Option<Arc<RenderImage>>,
    seen_camera_seq: u64,
    mic: Arc<Worker>,
    /// f32 bits: mic level in dBFS before gain.
    mic_db: Arc<AtomicU32>,
    seen_mic: u32,
    seen_states: String,
    _pump: Task<()>,
}

impl Preview {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pump = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            if this.update_in(cx, |this, window, cx| this.pump(window, cx)).is_err() {
                break;
            }
        });
        Self {
            screen: None,
            screen_state: Feed::Off,
            screen_task: None,
            camera: Worker::new(),
            camera_image: None,
            seen_camera_seq: 0,
            mic: Worker::new(),
            mic_db: Arc::new(AtomicU32::new(f32::NEG_INFINITY.to_bits())),
            seen_mic: 0,
            seen_states: String::new(),
            _pump: pump,
        }
    }

    // --- screen ---------------------------------------------------------

    /// Capture display number `number` (the N in "Capture screen N").
    pub fn start_screen(&mut self, number: usize, cx: &mut Context<Self>) {
        self.screen = None;
        self.screen_state = Feed::Starting;
        let sources = cx.screen_capture_sources();
        self.screen_task = Some(cx.spawn(async move |this, cx| {
            let outcome = async {
                let sources = sources.await.map_err(|_| "screen capture was cancelled".to_string())?.map_err(|e| format!("{e:#}"))?;
                let source = sources
                    .get(number)
                    .or_else(|| sources.first())
                    .ok_or_else(|| "no display can be captured".to_string())?;
                let latest: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
                let slot = latest.clone();
                let converter = Mutex::new(Converter::new()?);
                let started = source.stream(
                    cx.foreground_executor(),
                    Box::new(move |frame| {
                        // SAFETY: the pointer comes from a live CFRetained buffer; the
                        // get rule takes our own retain before `frame` is dropped.
                        let ptr = &*frame.0 as *const _ as *const std::ffi::c_void;
                        let buffer = unsafe { CVPixelBuffer::wrap_under_get_rule(ptr as _) };
                        if let Some(painted) = lock(&converter).convert(&buffer) {
                            *lock(&slot) = Some(Frame(painted));
                        }
                    }),
                );
                let stream = started
                    .await
                    .map_err(|_| "screen capture was cancelled".to_string())?
                    .map_err(|e| format!("{e:#}. Allow Screen Recording for your terminal in System Settings."))?;
                Ok::<_, String>(ScreenFeed { _stream: stream, latest })
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(feed) => {
                        this.screen = Some(feed);
                        this.screen_state = Feed::Running;
                    }
                    Err(message) => this.screen_state = Feed::Failed(message),
                }
                cx.notify();
            });
        }));
    }

    /// The newest screen frame, ready to paint.
    pub fn screen_frame(&self) -> Option<CVPixelBuffer> {
        let feed = self.screen.as_ref()?;
        let guard = lock(&feed.latest);
        guard.as_ref().map(|frame| frame.0.full.clone())
    }

    /// A tiny copy of the newest frame; stretched, it is the blur backdrop.
    pub fn screen_thumb(&self) -> Option<CVPixelBuffer> {
        let feed = self.screen.as_ref()?;
        let guard = lock(&feed.latest);
        guard.as_ref().map(|frame| frame.0.thumb.clone())
    }

    pub fn screen_state(&self) -> Feed {
        self.screen_state.clone()
    }

    // --- camera ---------------------------------------------------------

    pub fn start_camera(&mut self, cfg: &Config) {
        let generation = self.camera.restart();
        let worker = self.camera.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            if let Err(message) = run_camera(&worker, generation, &cfg) {
                worker.fail(generation, message);
            }
        });
    }

    /// Release the camera (the stream needs it while live).
    pub fn stop_camera(&mut self) {
        self.camera.stop();
    }

    pub fn camera_image(&self) -> Option<Arc<RenderImage>> {
        self.camera_image.clone()
    }

    pub fn camera_state(&self) -> Feed {
        lock(&self.camera.state).clone()
    }

    // --- mic ------------------------------------------------------------

    pub fn start_mic(&mut self, cfg: &Config) {
        let generation = self.mic.restart();
        self.mic_db.store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        if cfg.audio.mic_device.trim().is_empty() {
            self.mic.stop();
            return;
        }
        let worker = self.mic.clone();
        let level = self.mic_db.clone();
        let device = cfg.audio.mic_device.clone();
        std::thread::spawn(move || {
            if let Err(message) = run_mic(&worker, generation, &level, &device) {
                worker.fail(generation, message);
            }
        });
    }

    /// Mic level in dBFS (before gain); -inf when silent or off.
    pub fn mic_db(&self) -> f32 {
        f32::from_bits(self.mic_db.load(Ordering::Relaxed))
    }

    // --- pump -----------------------------------------------------------

    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dirty = false;
        let seq = self.camera.frame_seq.load(Ordering::Acquire);
        if seq != self.seen_camera_seq {
            self.seen_camera_seq = seq;
            if let Some(bgra) = lock(&self.camera.frame).take() {
                if let Some(buf) = image::RgbaImage::from_raw(CAMERA_SIZE.0, CAMERA_SIZE.1, bgra) {
                    let next = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
                    if let Some(old) = self.camera_image.replace(next) {
                        let _ = window.drop_image(old);
                    }
                    dirty = true;
                }
            }
        }
        let mic = self.mic_db.load(Ordering::Relaxed);
        if mic != self.seen_mic {
            self.seen_mic = mic;
            dirty = true;
        }
        let states = format!("{:?}{:?}", self.camera_state(), self.screen_state);
        if states != self.seen_states {
            self.seen_states = states;
            dirty = true;
        }
        if dirty {
            cx.notify();
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.camera.stop();
        self.mic.stop();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Camera: one ffmpeg process writing fixed-size BGRA frames to a pipe. The
/// newest frame wins, so a slow UI never builds a backlog.
fn run_camera(worker: &Arc<Worker>, generation: u64, cfg: &Config) -> Result<(), String> {
    let list = devices::probe_avfoundation().map_err(|e| format!("{e:#}"))?;
    let index = list
        .resolve_video(&cfg.camera.video_device)
        .ok_or_else(|| format!("camera {:?} is not connected. Pick another one.", cfg.camera.video_device))?;
    let modes = devices::camera_modes(&index);
    let (w, h) = CAMERA_SIZE;
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "avfoundation", "-pixel_format", "uyvy422"]
        .map(String::from)
        .into();
    let fps = match devices::pick_camera_mode(&modes, CAMERA_FPS, CAMERA_SIZE) {
        Some((mw, mh, mfps)) => {
            args.extend(["-video_size".into(), format!("{mw}x{mh}")]);
            mfps
        }
        None => CAMERA_FPS,
    };
    args.extend([
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        format!("{index}:"),
        "-vf".into(),
        format!("scale={w}:{h}:force_original_aspect_ratio=increase:flags=bilinear,crop={w}:{h}"),
        "-pix_fmt".into(),
        "bgra".into(),
        "-f".into(),
        "rawvideo".into(),
        "pipe:1".into(),
    ]);
    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}. Is it installed (brew install ffmpeg)?"))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("ffmpeg started without pipes".into());
    };
    if !worker.adopt(child, generation) {
        return Ok(());
    }
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let mut first = true;
    while stdout.read_exact(&mut buf).is_ok() {
        if !worker.current(generation) {
            return Ok(());
        }
        if first {
            *lock(&worker.state) = Feed::Running;
            first = false;
        }
        *lock(&worker.frame) = Some(std::mem::replace(&mut buf, vec![0u8; (w * h * 4) as usize]));
        worker.frame_seq.fetch_add(1, Ordering::Release);
    }
    if !worker.current(generation) {
        return Ok(());
    }
    let mut log = String::new();
    let _ = stderr.read_to_string(&mut log);
    let reason = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output from ffmpeg");
    Err(format!("camera stopped: {reason}. Check camera permission for your terminal."))
}

/// Mic level meter: ffmpeg reads the mic, we compute RMS per 50 ms window.
fn run_mic(worker: &Arc<Worker>, generation: u64, level: &AtomicU32, device: &str) -> Result<(), String> {
    let list = devices::probe_avfoundation().map_err(|e| format!("{e:#}"))?;
    let index = list.resolve_audio(device).ok_or_else(|| format!("mic {device:?} is not connected. Pick another one."))?;
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "avfoundation", "-i"])
        .arg(format!(":{index}"))
        .args(["-ac", "1", "-ar", "16000", "-f", "f32le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        return Err("ffmpeg started without a pipe".into());
    };
    if !worker.adopt(child, generation) {
        return Ok(());
    }
    *lock(&worker.state) = Feed::Running;
    let mut bytes = vec![0u8; METER_SAMPLES * 4];
    while stdout.read_exact(&mut bytes).is_ok() && worker.current(generation) {
        let sum: f32 = bytes
            .chunks_exact(4)
            .map(|b| {
                let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                s * s
            })
            .sum();
        let db = 10.0 * (sum / METER_SAMPLES as f32).max(1e-10).log10();
        level.store(db.to_bits(), Ordering::Relaxed);
    }
    Ok(())
}
