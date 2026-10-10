//! `minicast gui`: a native setup + preview window. The CLI stays the engine:
//! this window edits the same config file and starts/stops streams through the
//! same registry, so anything done here shows up in `minicast stream list`.
//!
//! Speed rule: layout (fit, camera position and size) is drawn here from the
//! config, so it changes instantly. Only a device change restarts a capture.
mod convert;
mod preview;
mod state;
mod theme;

use crate::config::{self, CameraConfig, Config};
use crate::devices::DeviceList;
use crate::streams;
use core_video::pixel_buffer::CVPixelBuffer;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, StyledExt, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use preview::{Feed, Preview};
use state::{Health, Phase, Source};
use std::path::PathBuf;
use std::time::Duration;

const PANEL_WIDTH: f32 = 320.0;
const BAR_HEIGHT: f32 = 52.0;
const FOOTER_HEIGHT: f32 = 36.0;
const GUTTER: f32 = 24.0;
/// Mic meter floor; anything quieter reads as empty.
const METER_FLOOR_DB: f32 = -60.0;
/// Cells in the mic meter.
const METER_CELLS: usize = 36;
/// Side of a resize dot on the canvas, in px.
const DOT: f32 = 10.0;
/// Smallest camera box width, in stream pixels.
const MIN_CAMERA_W: f32 = 120.0;

const FITS: [(&str, &str); 5] = [
    ("match", "Match"),
    ("stretch", "Stretch"),
    ("blur", "Blur"),
    ("cover", "Cover"),
    ("contain", "Contain"),
];
const CORNERS: [(&str, &str); 4] = [
    ("top-left", "TL"),
    ("top-right", "TR"),
    ("bottom-left", "BL"),
    ("bottom-right", "BR"),
];

/// What an edit means for a running stream.
#[derive(Clone, Copy)]
enum Change {
    /// Camera position or size: sent live, no reconnect.
    Camera,
    /// Mic level: sent live, no reconnect.
    MicGain,
    /// System sound level: sent live, no reconnect.
    SystemGain,
    /// Fit, devices, camera on/off, system sound on/off: ffmpeg's graph or
    /// output has to change, so these wait for an explicit Apply.
    Restart,
}

/// Which source list is expanded in the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Screen,
    Camera,
    Mic,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    const ALL: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

    fn is_right(self) -> bool {
        matches!(self, Corner::TopRight | Corner::BottomRight)
    }

    fn is_bottom(self) -> bool {
        matches!(self, Corner::BottomLeft | Corner::BottomRight)
    }
}

#[derive(Clone, Copy)]
enum Handle {
    Move,
    Resize(Corner),
}

/// A rectangle in stream pixels (the canvas the encoder sees).
#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// A camera box drag in progress. Positions are window pixels; `scale` turns
/// them into stream pixels.
struct Drag {
    handle: Handle,
    origin: Point<Pixels>,
    start: Rect,
    canvas: (f32, f32),
    scale: f32,
    moved: bool,
}

struct Shell {
    cfg_path: PathBuf,
    cfg: Config,
    phase: Phase,
    preview: Entity<Preview>,
    mic_gain: Entity<SliderState>,
    system_gain: Entity<SliderState>,
    /// Capture devices found by the probe (empty until it finishes).
    devices: DeviceList,
    picker: Option<Picker>,
    drag: Option<Drag>,
    notice: Option<String>,
    /// Edits the running stream cannot take live; "Apply" restarts it.
    restart_pending: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cfg_path = config::resolve_config_path(None);
        let (cfg, notice) = match Config::load_or_init(&cfg_path) {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                Config::default(),
                Some(format!("could not read {}: {e:#}. Showing defaults; nothing will be saved over it.", cfg_path.display())),
            ),
        };
        let preview = cx.new(|cx| Preview::new(window, cx));
        let gain_slider = |db: f32, cx: &mut Context<Self>| {
            cx.new(|_| SliderState::new().min(-30.0).max(30.0).step(1.0).default_value(db))
        };
        let mic_gain = gain_slider(cfg.audio.mic_gain_db, cx);
        let system_gain = gain_slider(cfg.audio.system_gain_db, cx);

        let mut subscriptions = vec![cx.observe(&preview, |_, _, cx| cx.notify())];
        subscriptions.push(cx.subscribe(&mic_gain, |this, _, event: &SliderEvent, cx| {
            match event {
                SliderEvent::Change(v) => this.cfg.audio.mic_gain_db = v.start().round(),
                SliderEvent::Release(_) => this.commit(Change::MicGain, cx),
            }
            cx.notify();
        }));
        subscriptions.push(cx.subscribe(&system_gain, |this, _, event: &SliderEvent, cx| {
            match event {
                SliderEvent::Change(v) => this.cfg.audio.system_gain_db = v.start().round(),
                SliderEvent::Release(_) => this.commit(Change::SystemGain, cx),
            }
            cx.notify();
        }));

        let poll = cx.spawn(async move |this, cx| loop {
            let phase = cx.background_spawn(async { state::poll() }).await;
            if this.update(cx, |this, cx| this.apply_poll(phase, cx)).is_err() {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(1)).await;
        });

        let mut shell = Self {
            cfg_path,
            cfg,
            phase: Phase::Offline,
            preview,
            mic_gain,
            system_gain,
            devices: DeviceList::default(),
            picker: None,
            drag: None,
            notice,
            restart_pending: false,
            _tasks: vec![poll],
            _subscriptions: subscriptions,
        };
        shell.rescan(cx);
        shell
    }

    /// Probe capture devices again (after plugging one in, say), then start
    /// every feed, since feeds need the resolved device names.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        crate::devices::forget_probes();
        cx.spawn(async move |this, cx| {
            let list = cx.background_spawn(async { crate::devices::probe_avfoundation() }).await;
            let _ = this.update(cx, |this, cx| {
                match list {
                    Ok(list) => this.devices = list,
                    Err(e) => this.notice = Some(format!("could not list devices: {e:#}")),
                }
                this.restart_screen(cx);
                this.restart_camera(cx);
                this.restart_mic(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.cfg.save(&self.cfg_path) {
            self.notice = Some(format!("could not save {}: {e:#}", self.cfg_path.display()));
        }
        cx.notify();
    }

    /// Save a setting. If a stream is running, the change is either sent to it
    /// live or marked as needing a restart (see `Change`).
    fn commit(&mut self, change: Change, cx: &mut Context<Self>) {
        self.save(cx);
        let Phase::Live(live) = &self.phase else { return };
        let Source::Detached { id } = &live.source else {
            self.notice = Some(
                "This stream was started from a terminal, so it keeps its old settings. \
                 End it and press Go live to use the new ones."
                    .into(),
            );
            return;
        };
        let commands = match change {
            Change::Restart => {
                self.restart_pending = true;
                return;
            }
            Change::Camera => {
                let screen = self.preview.read(cx).screen_frame().map(|b| (b.get_width() as f32, b.get_height() as f32));
                let r = camera_rect(&self.cfg.camera, self.canvas_size(screen));
                let (x, y, w, h) = (r.x.round() as i32, r.y.round() as i32, r.w.round() as i32, r.h.round() as i32);
                // Scale before crop: the crop box can only be as big as its input.
                vec![
                    format!("scale@cam w {w}"),
                    format!("scale@cam h {h}"),
                    format!("crop@cam w {w}"),
                    format!("crop@cam h {h}"),
                    format!("overlay@cam x {x}"),
                    format!("overlay@cam y {y}"),
                ]
            }
            Change::MicGain => vec![format!("volume@mic volume {}dB", self.cfg.audio.mic_gain_db)],
            Change::SystemGain => vec![format!("volume@sys volume {}dB", self.cfg.audio.system_gain_db)],
        };
        let sent = streams::find_record(id).and_then(|rec| {
            let rec = rec.ok_or_else(|| anyhow::anyhow!("stream {id} is not running any more"))?;
            commands.iter().try_for_each(|command| streams::send_command(&rec, command))
        });
        if let Err(e) = sent {
            self.notice = Some(format!("could not change the live stream: {e:#}"));
        }
    }

    /// Restart the running stream from the saved config, for the changes ffmpeg
    /// cannot take live (fit, devices, camera on/off, system sound). The
    /// bitrate is kept from the running stream: re-measuring the uplink would
    /// compete with it.
    fn apply_live(&mut self, cx: &mut Context<Self>) {
        self.restart_pending = false;
        let Phase::Live(live) = self.phase.clone() else { return };
        let Source::Detached { id } = live.source.clone() else { return };
        self.phase = Phase::Applying;
        self.notice = None;
        let path = self.cfg_path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let kbps = streams::find_record(&id)?
                        .and_then(|rec| Config::load_raw(std::path::Path::new(&rec.snapshot_path)).ok())
                        .map(|old| old.video.bitrate_kbps);
                    stop_stream(&live.source)?;
                    start_stream(&path, kbps)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.phase = Phase::Starting,
                    Err(e) => {
                        this.notice = Some(format!("could not apply the change to the stream: {e:#}"));
                        this.phase = state::poll();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The N in "Capture screen N" for the configured screen.
    fn screen_number(&self) -> usize {
        self.devices
            .resolve_video(&self.cfg.screen.video_device)
            .and_then(|index| self.devices.video.iter().find(|(i, _)| *i == index).map(|(_, name)| name.clone()))
            .and_then(|name| name.rsplit(' ').next().and_then(|n| n.parse().ok()))
            .unwrap_or(0)
    }

    fn restart_screen(&mut self, cx: &mut Context<Self>) {
        let number = self.screen_number();
        self.preview.update(cx, |p, cx| p.start_screen(number, cx));
    }

    /// The camera feed keeps running while live, so the box on the canvas
    /// shows the real picture and stays editable.
    fn restart_camera(&mut self, cx: &mut Context<Self>) {
        let cfg = self.cfg.clone();
        self.preview.update(cx, |p, _| if cfg.camera.enabled { p.start_camera(&cfg) } else { p.stop_camera() });
    }

    fn restart_mic(&mut self, cx: &mut Context<Self>) {
        let cfg = self.cfg.clone();
        self.preview.update(cx, |p, _| p.start_mic(&cfg));
    }

    fn apply_poll(&mut self, polled: Phase, cx: &mut Context<Self>) {
        match (&self.phase, polled) {
            // A start/stop/apply job is still running; the registry lags behind it.
            (Phase::Starting, Phase::Offline) | (Phase::Stopping, Phase::Live(_)) | (Phase::Applying, _) => {}
            (_, next) => self.phase = next,
        }
        cx.notify();
    }

    fn go_live(&mut self, cx: &mut Context<Self>) {
        self.restart_pending = false;
        self.phase = Phase::Starting;
        self.notice = None;
        crate::devices::forget_probes();
        let path = self.cfg_path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { start_stream(&path, None) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.phase = Phase::Offline;
                    this.notice = Some(format!("could not start the stream: {e:#}"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn end_stream(&mut self, source: Source, cx: &mut Context<Self>) {
        self.restart_pending = false;
        self.phase = Phase::Stopping;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { stop_stream(&source) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.phase = Phase::Offline,
                    Err(e) => {
                        this.notice = Some(format!("could not stop the stream: {e:#}. It may still be running."));
                        this.phase = state::poll();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Loopback device (BlackHole etc.) usable for system sound, if any.
    fn loopback(&self) -> Option<String> {
        self.devices
            .audio
            .iter()
            .map(|(_, name)| name)
            .find(|n| ["blackhole", "loopback", "soundflower"].iter().any(|k| n.to_lowercase().contains(k)))
            .cloned()
    }

    /// The canvas the stream would use, in stream pixels. `match` follows the
    /// screen's own shape; every other fit uses the configured size.
    fn canvas_size(&self, screen: Option<(f32, f32)>) -> (f32, f32) {
        let ch = self.cfg.video.height as f32;
        let configured = self.cfg.video.width as f32 / ch.max(1.0);
        let aspect = match screen {
            Some((w, h)) if self.cfg.screen.fit == "match" => w / h.max(1.0),
            _ => configured,
        };
        (((ch * aspect) / 2.0).round() * 2.0, ch)
    }

    fn begin_drag(&mut self, handle: Handle, origin: Point<Pixels>, start: Rect, canvas: (f32, f32), scale: f32) {
        self.drag = Some(Drag { handle, origin, start, canvas, scale, moved: false });
    }

    /// Move or resize the camera box. The box keeps its shape when resizing
    /// and stays inside the canvas.
    fn drag_to(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.as_mut() else { return };
        let dx = f32::from(pos.x - drag.origin.x) / drag.scale;
        let dy = f32::from(pos.y - drag.origin.y) / drag.scale;
        let (cw, ch) = drag.canvas;
        let s = drag.start;
        let (x, y, w, h) = match drag.handle {
            Handle::Move => (s.x + dx, s.y + dy, s.w, s.h),
            Handle::Resize(corner) => {
                let grow = if corner.is_right() { dx } else { -dx };
                let w = (s.w + grow).clamp(MIN_CAMERA_W, (cw * 0.6).max(MIN_CAMERA_W));
                let h = w * s.h / s.w;
                let x = if corner.is_right() { s.x } else { s.x + s.w - w };
                let y = if corner.is_bottom() { s.y } else { s.y + s.h - h };
                (x, y, w, h)
            }
        };
        let x = x.clamp(0.0, (cw - w).max(0.0));
        let y = y.clamp(0.0, (ch - h).max(0.0));
        // H.264 needs even sizes, and even offsets keep chroma aligned.
        let even = |v: f32| ((v / 2.0).round() as i32) * 2;
        let cam = &mut self.cfg.camera;
        cam.corner = "custom".into();
        cam.custom_x = even(x);
        cam.custom_y = even(y);
        cam.width = even(w).max(2) as u32;
        cam.height = even(h).max(2) as u32;
        drag.moved = true;
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if drag.moved {
                self.commit(Change::Camera, cx);
            }
        }
    }
}

/// Start a detached stream from the saved config. `keep_kbps` pins the
/// bitrate (skipping the uplink speed test) when restarting a running stream.
fn start_stream(path: &std::path::Path, keep_kbps: Option<u32>) -> anyhow::Result<()> {
    let mut cfg = Config::load(path)?;
    if let Some(kbps) = keep_kbps {
        cfg.video.bitrate_kbps = kbps;
        cfg.video.bitrate_auto = false;
    }
    crate::ffmpeg::resolve_media(&mut cfg)?;
    let argv = crate::ffmpeg::build_argv(&cfg)?;
    let retry = streams::RetryPolicy::from_config(&cfg);
    streams::spawn_detached(None, &cfg, &argv, retry, cfg.audio.mic_capture.as_ref())?;
    Ok(())
}

fn stop_stream(source: &Source) -> anyhow::Result<()> {
    let (pid, stopped) = match source {
        Source::Detached { id } => {
            let Some(rec) = streams::remove_record(id)? else { return Ok(()) };
            let stopped = if rec.supervised { streams::terminate_tree(rec.pid)? } else { streams::terminate(rec.pid)? };
            (rec.pid, stopped)
        }
        // SIGTERM makes the terminal run end without retrying (see run_foreground).
        Source::Terminal { pid } => (*pid, streams::terminate(*pid)?),
    };
    anyhow::ensure!(stopped, "process {pid} did not exit");
    Ok(())
}

/// Where the camera box sits on a canvas of `canvas` stream pixels.
fn camera_rect(cam: &CameraConfig, (cw, ch): (f32, f32)) -> Rect {
    let (w, h) = (cam.width as f32, cam.height as f32);
    let (mx, my) = (cam.margin_x as f32, cam.margin_y as f32);
    let (x, y) = match cam.corner.as_str() {
        "top-left" => (mx, my),
        "top-right" => (cw - w - mx, my),
        "bottom-left" => (mx, ch - h - my),
        "custom" => (cam.custom_x as f32, cam.custom_y as f32),
        _ => (cw - w - mx, ch - h - my),
    };
    Rect { x, y, w, h }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .child(self.top_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.stage(window, cx))
                    .child(self.panel(cx)),
            )
            .child(self.footer(cx))
    }
}

impl Shell {
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (dot, label, label_color) = match &self.phase {
            Phase::Offline => (theme.muted_foreground, "OFFLINE".to_string(), theme.muted_foreground),
            Phase::Starting => (theme.warning, "STARTING".to_string(), theme.warning),
            Phase::Applying => (theme.warning, "APPLYING".to_string(), theme.warning),
            Phase::Stopping => (theme.warning, "STOPPING".to_string(), theme.warning),
            Phase::Live(live) => match live.health {
                Health::Stalled => (theme.warning, "NO DATA".to_string(), theme.warning),
                Health::Warming => (theme.warning, "CONNECTING".to_string(), theme.warning),
                Health::Sending { .. } | Health::Unmonitored => (
                    theme.danger,
                    format!("LIVE  {}", streams::format_uptime(live.started_at, streams::unix_now())),
                    theme.danger,
                ),
            },
        };
        let action = match &self.phase {
            Phase::Offline => Button::new("go-live")
                .primary()
                .label("Go live")
                .on_click(cx.listener(|this, _, _, cx| this.go_live(cx)))
                .into_any_element(),
            Phase::Live(live) => {
                let source = live.source.clone();
                let can_apply = self.restart_pending && matches!(live.source, Source::Detached { .. });
                h_flex()
                    .gap_2()
                    .when(can_apply, |d| {
                        d.child(
                            Button::new("apply")
                                .primary()
                                .outline()
                                .label("Apply changes (reconnects)")
                                .on_click(cx.listener(|this, _, _, cx| this.apply_live(cx))),
                        )
                    })
                    .child(
                        Button::new("end")
                            .danger()
                            .label("End stream")
                            .on_click(cx.listener(move |this, _, _, cx| this.end_stream(source.clone(), cx))),
                    )
                    .into_any_element()
            }
            Phase::Applying => Button::new("busy").label("Applying").disabled(true).into_any_element(),
            Phase::Starting => Button::new("busy").label("Starting").disabled(true).into_any_element(),
            Phase::Stopping => Button::new("busy").label("Stopping").disabled(true).into_any_element(),
        };
        h_flex()
            .h(px(BAR_HEIGHT))
            .flex_none()
            .items_center()
            .justify_between()
            .pl(px(84.0))
            .pr(px(GUTTER))
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_5()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.0)).bg(theme.primary))
                            .child(div().font_semibold().child("minicast")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(6.0)).bg(dot))
                            .child(div().text_xs().text_color(label_color).child(label)),
                    ),
            )
            .child(action)
    }

    /// The canvas: the screen at native resolution with the camera on top.
    /// Drag the camera to move it, its corner dots to resize it.
    fn stage(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let preview = self.preview.read(cx);
        let screen = preview.screen_frame();
        let screen_px = screen.as_ref().map(|b| (b.get_width() as f32, b.get_height() as f32));
        let camera_image = preview.camera_image();
        let screen_state = preview.screen_state();
        let thumb = preview.screen_thumb();
        let canvas = self.canvas_size(screen_px);
        let aspect = canvas.0 / canvas.1.max(1.0);

        let view = window.viewport_size();
        let free_w = (f32::from(view.width) - PANEL_WIDTH - 2.0 * GUTTER).max(160.0);
        let free_h = (f32::from(view.height) - BAR_HEIGHT - FOOTER_HEIGHT - 2.0 * GUTTER).max(90.0);
        let (w, h) = if free_w / free_h > aspect { (free_h * aspect, free_h) } else { (free_w, free_w / aspect) };
        let scale = w / canvas.0;

        let mut frame = div().relative().size_full().overflow_hidden().bg(theme.sidebar);
        if let Some(buffer) = screen {
            // Redraw every frame while capturing, at the display's own rate.
            window.request_animation_frame();
            frame = frame.child(screen_layer(buffer, thumb, &self.cfg.screen.fit));
        } else {
            let message = match screen_state {
                Feed::Failed(message) => message,
                _ => "Starting capture".to_string(),
            };
            frame = frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_6()
                    .text_color(theme.muted_foreground)
                    .child(message),
            );
        }
        if self.cfg.camera.enabled {
            let rect = camera_rect(&self.cfg.camera, canvas);
            let mut tile = div()
                .absolute()
                .left(px(rect.x * scale))
                .top(px(rect.y * scale))
                .w(px(rect.w * scale))
                .h(px(rect.h * scale))
                .overflow_hidden()
                .bg(theme.muted);
            if let Some(image) = camera_image {
                tile = tile.child(img(image).size_full().object_fit(ObjectFit::Cover));
            }
            frame = frame.child(tile).child(self.camera_box(rect, canvas, scale, cx));
        }
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.drag_to(event.position, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .child(div().w(px(w)).h(px(h)).border_1().border_color(theme.border).child(frame))
    }

    /// Selection outline for the camera: drag the body to move it, the corner
    /// dots to resize it.
    fn camera_box(&self, rect: Rect, canvas: (f32, f32), scale: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut outline = div()
            .absolute()
            .left(px(rect.x * scale))
            .top(px(rect.y * scale))
            .w(px(rect.w * scale))
            .h(px(rect.h * scale))
            .border_1()
            .border_color(theme.primary)
            .cursor_move()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    this.begin_drag(Handle::Move, event.position, rect, canvas, scale);
                }),
            );
        for corner in Corner::ALL {
            let mut dot = div().absolute().size(px(DOT)).bg(theme.primary).border_1().border_color(theme.foreground);
            dot = if corner.is_right() { dot.right(px(-DOT / 2.0)) } else { dot.left(px(-DOT / 2.0)) };
            dot = if corner.is_bottom() { dot.bottom(px(-DOT / 2.0)) } else { dot.top(px(-DOT / 2.0)) };
            dot = if corner.is_right() == corner.is_bottom() { dot.cursor_nwse_resize() } else { dot.cursor_nesw_resize() };
            outline = outline.child(dot.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.begin_drag(Handle::Resize(corner), event.position, rect, canvas, scale);
                    cx.stop_propagation();
                }),
            ));
        }
        outline
    }

    fn panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fit_row = segmented(FITS.iter().map(|&(value, label)| {
            self.segment(format!("fit-{value}"), label, self.cfg.screen.fit == value, Change::Restart, cx, move |this| {
                this.cfg.screen.fit = value.to_string();
            })
        }).collect::<Vec<_>>(), &theme);
        let corner_row = segmented(CORNERS.iter().map(|&(value, label)| {
            self.segment(format!("corner-{value}"), label, self.cfg.camera.corner == value, Change::Camera, cx, move |this| {
                this.cfg.camera.corner = value.to_string();
            })
        }).collect::<Vec<_>>(), &theme);
        let camera_on = self.cfg.camera.enabled;
        let system_on = !self.cfg.audio.system_device.trim().is_empty();
        let loopback = self.loopback();
        let screen_name = self.cfg.screen.video_device.clone();
        let camera_name = self.cfg.camera.video_device.clone();

        v_flex()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .h_full()
            .id("panel")
            .overflow_y_scroll()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .child(section(
                "Screen",
                &theme,
                v_flex()
                    .gap_3()
                    .child(self.source_row(Picker::Screen, screen_name, cx))
                    .child(fit_row),
            ))
            .child(section(
                "Camera",
                &theme,
                v_flex()
                    .gap_3()
                    .child(
                        Switch::new("camera-on")
                            .label("Show camera")
                            .checked(camera_on)
                            .on_change(cx.listener(|this, value: &bool, _, cx| {
                                this.cfg.camera.enabled = *value;
                                this.commit(Change::Restart, cx);
                                this.restart_camera(cx);
                            })),
                    )
                    .when(camera_on, |d| {
                        d.child(self.source_row(Picker::Camera, camera_name, cx))
                            .child(corner_row)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Drag the box on the canvas to move it. Drag a dot to resize."),
                            )
                    }),
            ))
            .child(section(
                "Audio",
                &theme,
                v_flex()
                    .gap_4()
                    .child(self.mic_block(cx))
                    .child(
                        v_flex()
                            .gap_3()
                            .child(match loopback {
                                Some(device) => Switch::new("system-on")
                                    .label("System sound")
                                    .checked(system_on)
                                    .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                        this.cfg.audio.system_device = if *value { device.clone() } else { String::new() };
                                        this.commit(Change::Restart, cx);
                                    }))
                                    .into_any_element(),
                                None => div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("System sound needs a loopback device such as BlackHole.")
                                    .into_any_element(),
                            })
                            .when(system_on, |d| d.child(Slider::new(&self.system_gain))),
                    ),
            ))
            .child(
                div().p(px(GUTTER)).child(
                    div()
                        .id("rescan")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .cursor_pointer()
                        .hover(|d| d.text_color(theme.primary))
                        .on_click(cx.listener(|this, _, _, cx| this.rescan(cx)))
                        .child("Rescan devices"),
                ),
            )
    }

    /// One source: its current device, which expands into the list of choices.
    fn source_row(&self, which: Picker, current: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let open = self.picker == Some(which);
        let shown = self.display_name(which, &current);
        let options: Vec<(String, String)> = match which {
            Picker::Screen => self.devices.screens(),
            Picker::Camera => self.devices.cameras(),
            Picker::Mic => self.devices.audio.clone(),
        };
        v_flex()
            .child(
                h_flex()
                    .id(SharedString::from(format!("source-{}", which as u8)))
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_1()
                    .border_color(if open { theme.primary } else { theme.input })
                    .bg(theme.background)
                    .cursor_pointer()
                    .hover(|d| d.border_color(theme.primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.picker = if this.picker == Some(which) { None } else { Some(which) };
                        cx.notify();
                    }))
                    .child(div().overflow_hidden().text_ellipsis().child(shown))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if open { theme.primary } else { theme.muted_foreground })
                            .child(if open { "Close" } else { "Change" }),
                    ),
            )
            .when(open, |d| {
                d.child(
                    v_flex()
                        .border_x_1()
                        .border_b_1()
                        .border_color(theme.primary)
                        .bg(theme.background)
                        .when(which == Picker::Mic, |d| {
                            d.child(self.option_row(which, "Off".into(), String::new(), current.trim().is_empty(), cx))
                        })
                        .children(options.into_iter().map(|(index, name)| {
                            let active = current == name || current == index;
                            self.option_row(which, name.clone(), name, active, cx)
                        })),
                )
            })
    }

    fn option_row(&self, which: Picker, label: String, value: String, active: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id(SharedString::from(format!("option-{}-{label}", which as u8)))
            .px_3()
            .py_2()
            .cursor_pointer()
            .overflow_hidden()
            .text_ellipsis()
            .when(active, |d| d.text_color(theme.primary).bg(theme.primary.opacity(0.12)))
            .when(!active, |d| d.text_color(theme.muted_foreground).hover(|d| d.text_color(theme.foreground).bg(theme.muted)))
            .on_click(cx.listener(move |this, _, _, cx| {
                match which {
                    Picker::Screen => {
                        this.cfg.screen.video_device = value.clone();
                        this.commit(Change::Restart, cx);
                        this.restart_screen(cx);
                    }
                    Picker::Camera => {
                        this.cfg.camera.video_device = value.clone();
                        this.commit(Change::Restart, cx);
                        this.restart_camera(cx);
                    }
                    Picker::Mic => {
                        this.cfg.audio.mic_device = value.clone();
                        this.commit(Change::Restart, cx);
                        this.restart_mic(cx);
                    }
                }
                this.picker = None;
                cx.notify();
            }))
            .child(label)
    }

    /// Device name for a stored value (older configs store a numeric index).
    fn display_name(&self, which: Picker, stored: &str) -> String {
        if stored.trim().is_empty() {
            return "Off".into();
        }
        let list = match which {
            Picker::Screen | Picker::Camera => &self.devices.video,
            Picker::Mic => &self.devices.audio,
        };
        list.iter()
            .find(|(index, _)| index == stored)
            .map_or_else(|| stored.to_string(), |(_, name)| name.clone())
    }

    fn mic_block(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let gain = self.cfg.audio.mic_gain_db;
        let db = self.preview.read(cx).mic_db() + gain;
        let level = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
        let lit = (level * METER_CELLS as f32).round() as usize;
        let cell = |i: usize| {
            let zone = i as f32 / METER_CELLS as f32;
            let color = if zone > 0.9 {
                theme.danger
            } else if zone > 0.75 {
                theme.warning
            } else {
                theme.primary
            };
            div().flex_1().h(px(8.0)).bg(if i < lit { color } else { theme.muted })
        };
        v_flex()
            .gap_3()
            .child(self.source_row(Picker::Mic, self.cfg.audio.mic_device.clone(), cx))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Level")
                            .child(format!("{gain:+.0} dB")),
                    )
                    .child(h_flex().gap(px(2.0)).children((0..METER_CELLS).map(cell)))
                    .child(Slider::new(&self.mic_gain)),
            )
    }

    fn segment(
        &self,
        id: String,
        label: &'static str,
        active: bool,
        change: Change,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self) + 'static,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id(SharedString::from(id))
            .flex_1()
            .flex()
            .justify_center()
            .py_2()
            .text_xs()
            .cursor_pointer()
            .border_b_2()
            .when(active, |d| d.text_color(theme.primary).border_color(theme.primary).bg(theme.primary.opacity(0.10)))
            .when(!active, |d| {
                d.text_color(theme.muted_foreground)
                    .border_color(theme.border)
                    .hover(|d| d.text_color(theme.foreground))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                apply(this);
                this.commit(change, cx);
            }))
            .child(label)
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let text = match (&self.notice, &self.phase) {
            (Some(n), _) => n.clone(),
            (None, Phase::Live(live)) => {
                let stats = match live.health {
                    Health::Sending { fps, speed } => format!("   {fps:.0} fps   {speed:.2}x"),
                    Health::Stalled => "   no data for 15 s".to_string(),
                    Health::Warming | Health::Unmonitored => String::new(),
                };
                format!("{}   {}{stats}", live.platform, live.output)
            }
            _ => {
                let screen = self.preview.read(cx).screen_frame().map(|b| (b.get_width() as f32, b.get_height() as f32));
                let (w, h) = self.canvas_size(screen);
                format!("{w:.0} x {h:.0}   {} fps   {} kbps", self.cfg.video.fps, self.cfg.video.bitrate_kbps)
            }
        };
        let color = if self.notice.is_some() { theme.danger } else { theme.muted_foreground };
        h_flex()
            .h(px(FOOTER_HEIGHT))
            .flex_none()
            .items_center()
            .px(px(GUTTER))
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(color)
            .child(text)
    }
}

/// The screen layer for each fit. The stream applies the same rules, so what
/// shows here is what viewers get. Blur stretches a tiny copy of the screen
/// behind the full picture, which is how the stream builds its backdrop too.
fn screen_layer(buffer: CVPixelBuffer, thumb: Option<CVPixelBuffer>, fit: &str) -> AnyElement {
    let fill = |buffer: CVPixelBuffer, object_fit: ObjectFit| surface(buffer).size_full().object_fit(object_fit);
    match (fit, thumb) {
        ("contain", _) => fill(buffer, ObjectFit::Contain).into_any_element(),
        ("cover", _) => fill(buffer, ObjectFit::Cover).into_any_element(),
        ("blur", Some(thumb)) => div()
            .relative()
            .size_full()
            .child(div().absolute().size_full().child(fill(thumb, ObjectFit::Cover)))
            .child(div().absolute().size_full().child(fill(buffer, ObjectFit::Contain)))
            .into_any_element(),
        _ => fill(buffer, ObjectFit::Fill).into_any_element(),
    }
}

/// Equal-width segments in one hairline-framed row.
fn segmented(segments: impl IntoIterator<Item = impl IntoElement>, theme: &Theme) -> impl IntoElement {
    h_flex().w_full().border_1().border_color(theme.border).bg(theme.background).children(segments)
}

fn section(label: &'static str, theme: &Theme, body: impl IntoElement) -> impl IntoElement {
    v_flex()
        .gap_4()
        .p(px(GUTTER))
        .border_b_1()
        .border_color(theme.border)
        .child(div().text_xs().font_semibold().text_color(theme.muted_foreground).child(label.to_uppercase()))
        .child(body)
}

pub fn run() {
    crate::devices::enable_probe_cache();
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        theme::apply(cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("minicast".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(18.0), px(18.0))),
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1240.0), px(760.0)), cx))),
            window_min_size: Some(size(px(960.0), px(620.0))),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| cx.new(|cx| Shell::new(window, cx))).expect("failed to open window");
        cx.activate(true);
    });
}
