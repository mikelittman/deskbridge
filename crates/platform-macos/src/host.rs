use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use deskbridge_proto::{AudioPacket, CodecId, Control, CursorPos, Input, VideoFrame};
use deskbridge_session::{mono_us, LinkHealth};

use crate::audio::AudioEncoder;
use crate::capture::{Capture, CapturedFrame, DisplayGeometry};
use crate::cg::mouse_global;
use crate::cursor::arrow_shape;
use crate::encode::Encoder;
use crate::error::PlatformError;
use crate::input::Injector;
use crate::pace::{encode_fps, picture_size, FramePace};
use crate::preview::{needs_preview, FrameFit, Preview};

pub struct HostOptions {
    pub display_index: usize,
    pub bitrate: u32,
    pub inject: bool,
    pub codec: CodecId,
    /// Largest capture size. The picture keeps the display aspect and is not scaled up.
    pub resolution: Option<(u32, u32)>,
}

pub enum Outbound {
    Control(Control),
    Frame(VideoFrame),
    Audio(AudioPacket),
}

pub struct Host {
    stop: Arc<AtomicBool>,
    input_tx: Option<Sender<Input>>,
    force_key: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    input_thread: Option<JoinHandle<()>>,
    video_backlog: Arc<AtomicUsize>,
    viewer_count: Arc<AtomicUsize>,
    link_health: Arc<LinkHealth>,
}

impl Host {
    pub fn spawn(options: HostOptions, outbound: UnboundedSender<Outbound>) -> Result<Self, PlatformError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let (geometry_tx, geometry_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let force_key = Arc::new(AtomicBool::new(true));
        let video_backlog = Arc::new(AtomicUsize::new(0));
        let viewer_count = Arc::new(AtomicUsize::new(1));
        let link_health = Arc::new(LinkHealth::new(options.bitrate));
        let stop_flag = Arc::clone(&stop);
        let force_flag = Arc::clone(&force_key);
        let backlog_flag = Arc::clone(&video_backlog);
        let viewers_flag = Arc::clone(&viewer_count);
        let link_flag = Arc::clone(&link_health);
        let inject = options.inject;
        let input_thread = thread::spawn(move || run_input(inject, geometry_rx, input_rx));
        let thread = thread::spawn(move || {
            if let Err(err) = run(
                options,
                outbound,
                geometry_tx,
                ready_tx,
                stop_flag,
                force_flag,
                backlog_flag,
                viewers_flag,
                link_flag,
            ) {
                eprintln!("deskbridge host: {err}");
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                input_tx: Some(input_tx),
                force_key,
                thread: Some(thread),
                input_thread: Some(input_thread),
                video_backlog,
                viewer_count,
                link_health,
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                drop(input_tx);
                let _ = input_thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                drop(input_tx);
                let _ = input_thread.join();
                Err(PlatformError::new("capture thread stopped before it started"))
            }
        }
    }

    pub fn inject(&self, input: Input) {
        if let Some(tx) = &self.input_tx {
            let _ = tx.send(input);
        }
    }

    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }

    /// Frames accepted by viewers and not yet acknowledged.
    pub fn video_backlog(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.video_backlog)
    }

    pub fn set_viewer_count(&self, count: usize) {
        self.viewer_count.store(count.max(1), Ordering::Relaxed);
    }

    pub fn link_health(&self) -> Arc<LinkHealth> {
        Arc::clone(&self.link_health)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Drop the sender before joining, or the input thread stays blocked in recv.
        drop(self.input_tx.take());
        if let Some(thread) = self.input_thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_input(inject: bool, geometry_rx: Receiver<DisplayGeometry>, input_rx: Receiver<Input>) {
    let Ok(geometry) = geometry_rx.recv() else {
        return;
    };
    let mut injector = Injector::new(geometry);
    while let Ok(input) = input_rx.recv() {
        if let Input::MouseMove { mut x, mut y } = input {
            let mut followed = None;
            loop {
                match input_rx.try_recv() {
                    Ok(Input::MouseMove { x: nx, y: ny }) => {
                        x = nx;
                        y = ny;
                    }
                    Ok(other) => {
                        followed = Some(other);
                        break;
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
            if inject {
                injector.apply(Input::MouseMove { x, y });
                if let Some(other) = followed {
                    injector.apply(other);
                }
            }
        } else if inject {
            injector.apply(input);
        }
    }
}

fn run(
    options: HostOptions,
    outbound: UnboundedSender<Outbound>,
    geometry_tx: Sender<DisplayGeometry>,
    ready: Sender<Result<(), PlatformError>>,
    stop: Arc<AtomicBool>,
    force_key: Arc<AtomicBool>,
    video_backlog: Arc<AtomicUsize>,
    viewer_count: Arc<AtomicUsize>,
    link: Arc<LinkHealth>,
) -> Result<(), PlatformError> {
    let capture = match Capture::start(options.display_index, options.resolution) {
        Ok(capture) => capture,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
    // A viewer who asked for a size should get that size, not the small first picture.
    let mut preview = if options.resolution.is_none() && needs_preview(capture.info.width, capture.info.height) {
        match Preview::open(options.codec, capture.info.width, capture.info.height, capture.fps) {
            Ok(preview) => Some(preview),
            Err(err) => {
                eprintln!("deskbridge host: small first picture is unavailable ({err})");
                None
            }
        }
    } else {
        None
    };
    let mut encoder = if preview.is_none() {
        match Encoder::open(
            options.codec,
            capture.info.width,
            capture.info.height,
            link.pace_bps(),
            capture.fps,
        ) {
            Ok(encoder) => Some(encoder),
            Err(err) => {
                let _ = ready.send(Err(PlatformError::new(err.to_string())));
                return Err(err);
            }
        }
    } else {
        None
    };
    let _ = geometry_tx.send(capture.geometry);
    let _ = outbound.send(Outbound::Control(Control::DisplayInfo(capture.info.clone())));
    let _ = outbound.send(Outbound::Control(Control::CursorShape(arrow_shape())));
    let _ = ready.send(Ok(()));
    let limit = match options.resolution {
        Some((width, height)) => format!(" (limit {width}x{height})"),
        None => String::new(),
    };
    eprintln!(
        "capturing display {} at {}x{} {} using hardware {}{}",
        options.display_index,
        capture.info.width,
        capture.info.height,
        if capture.info.refresh_millihz == 0 {
            "60 Hz".into()
        } else {
            format!("{:.0} Hz", f64::from(capture.info.refresh_millihz) / 1000.0)
        },
        options.codec.name(),
        limit
    );
    if let Some(preview) = &preview {
        eprintln!(
            "first picture is {}x{} so it can arrive quickly, then the full frame follows",
            preview.width, preview.height
        );
    }

    let mut pace = FramePace::new(options.bitrate, capture.fps);
    let mut shown_pace = options.bitrate;
    let mut audio_encoder: Option<AudioEncoder> = None;
    let mut audio_broken = false;
    let mut last_cursor: Option<(i32, i32, bool)> = None;
    // Kept so a viewer who joins a still screen can be sent one keyframe.
    // After that, a still screen sends nothing until the picture changes.
    let mut latest: Option<CapturedFrame> = None;
    let mut pending: Option<CapturedFrame> = None;
    let mut idle_encode_failed = false;
    let mut encoder_failed = false;
    let mut fit: Option<FrameFit> = None;
    let mut target_w = capture.info.width;
    let mut target_h = capture.info.height;
    let mut spatial = 0u8;
    while !stop.load(Ordering::Relaxed) {
        let wanted = link.spatial();
        if wanted != spatial && preview.is_none() {
            spatial = wanted;
            let (width, height) = picture_size(capture.info.width, capture.info.height, spatial);
            if width != target_w || height != target_h {
                target_w = width;
                target_h = height;
                encoder = None;
                fit = None;
                force_key.store(true, Ordering::Relaxed);
                eprintln!("picture {width}x{height} to match the link");
            }
        }
        if let Some(frame) = capture.wait_frame(Duration::from_millis(8)) {
            let unchanged = matches!(frame.changed, Some(fraction) if fraction <= 0.0);
            if !unchanged || (latest.is_none() && pending.is_none()) {
                pending = Some(frame);
            }
        }

        let now = Instant::now();
        let viewers = viewer_count.load(Ordering::Relaxed).max(1);
        let depth = link.depth().max(1);
        let link_busy = video_backlog.load(Ordering::Relaxed) >= viewers.saturating_mul(depth);
        let pace_bps = link.pace_bps();
        pace.set_bitrate(pace_bps);
        if shown_pace != 0 && pace_bps.abs_diff(shown_pace) * 4 > shown_pace {
            eprintln!(
                "link pace {:.1} Mbps, {} frame(s) in flight",
                f64::from(pace_bps) / 1_000_000.0,
                depth
            );
        }
        shown_pace = pace_bps;
        if preview.is_some() {
            if pending.is_some() && !link_busy {
                let frame = pending.take().expect("pending frame");
                let changed = frame.changed;
                let capture_us = frame.capture_us;
                let encoded = preview
                    .as_mut()
                    .expect("preview")
                    .encode(&frame.surface, capture_us);
                match encoded {
                    Ok(Some(packet)) => {
                        if let Some(bytes) = publish_packet(&outbound, packet) {
                            pace.observe(now, bytes, changed);
                            latest = Some(frame);
                            preview = None;
                        } else {
                            pending = Some(frame);
                            preview = None;
                        }
                    }
                    Ok(None) => {
                        pending = Some(frame);
                        preview = None;
                    }
                    Err(err) => {
                        eprintln!("preview: {err}");
                        pending = Some(frame);
                        preview = None;
                    }
                }
            }
        } else if encoder.is_none() && !encoder_failed {
            match Encoder::open(
                options.codec,
                target_w,
                target_h,
                link.pace_bps(),
                capture.fps,
            ) {
                Ok(opened) => encoder = Some(opened),
                Err(err) => {
                    eprintln!("encode: {err}");
                    encoder_failed = true;
                }
            }
        }
        if preview.is_none() {
            if let Some(encoder) = encoder.as_mut() {
                if pending.is_some()
                    && !link_busy
                    && (pace.due(now) || force_key.load(Ordering::Relaxed))
                {
                    let frame = pending.take().expect("pending frame");
                    let changed = frame.changed;
                    let capture_us = frame.capture_us;
                    let force = force_key.swap(false, Ordering::Relaxed);
                    aim_encoder(encoder, capture.fps, changed, link.pace_bps(), options.bitrate);
                    match emit_fitted(
                        encoder,
                        &mut fit,
                        target_w,
                        target_h,
                        &outbound,
                        &frame.surface,
                        capture_us,
                        force,
                        &force_key,
                    ) {
                        Some(bytes) => {
                            pace.observe(now, bytes, changed);
                            idle_encode_failed = false;
                            latest = Some(frame);
                        }
                        None => {
                            pace.observe(now, 0, changed);
                            if force {
                                force_key.store(true, Ordering::Relaxed);
                            }
                            pending = Some(frame);
                        }
                    }
                } else if pending.is_none()
                    && !link_busy
                    && !idle_encode_failed
                    && force_key.load(Ordering::Relaxed)
                {
                    if let Some(frame) = latest.as_ref() {
                        let changed = frame.changed;
                        let force = force_key.swap(false, Ordering::Relaxed);
                        if force {
                            aim_encoder(encoder, capture.fps, changed, link.pace_bps(), options.bitrate);
                            match emit_fitted(
                                encoder,
                                &mut fit,
                                target_w,
                                target_h,
                                &outbound,
                                &frame.surface,
                                mono_us(),
                                true,
                                &force_key,
                            ) {
                                Some(bytes) => pace.observe(now, bytes, changed),
                                None => {
                                    idle_encode_failed = true;
                                    force_key.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            }
        }
        publish_cursor(&outbound, capture.geometry, &mut last_cursor);
        drain_audio(&outbound, &capture, &mut audio_encoder, &mut audio_broken);
    }
    capture.stop();
    Ok(())
}

fn drain_audio(
    outbound: &UnboundedSender<Outbound>,
    capture: &Capture,
    encoder: &mut Option<AudioEncoder>,
    broken: &mut bool,
) {
    if *broken {
        return;
    }
    let Some(chunk) = capture.take_audio() else {
        return;
    };
    let same = encoder
        .as_ref()
        .is_some_and(|encoder| encoder.rate() == chunk.rate && encoder.channels() == chunk.channels);
    if !same {
        match AudioEncoder::open(chunk.rate, chunk.channels, 96_000) {
            Ok(opened) => {
                eprintln!(
                    "system audio {} Hz, {} channel(s)",
                    chunk.rate, chunk.channels
                );
                *encoder = Some(opened);
            }
            Err(err) => {
                eprintln!("audio: {err}");
                *encoder = None;
                *broken = true;
                return;
            }
        }
    }
    let Some(encoder) = encoder.as_mut() else {
        return;
    };
    let capture_us = mono_us();
    for payload in encoder.push(&chunk.interleaved) {
        let _ = outbound.send(Outbound::Audio(encoder.packet(payload, capture_us)));
    }
}

fn aim_encoder(
    encoder: &mut Encoder,
    display_fps: u32,
    changed: Option<f32>,
    pace_bps: u32,
    target_bps: u32,
) {
    encoder.set_target_fps(encode_fps(display_fps, changed, pace_bps, target_bps));
    encoder.set_bitrate(pace_bps);
}

fn emit_fitted(
    encoder: &mut Encoder,
    fit: &mut Option<FrameFit>,
    target_w: u32,
    target_h: u32,
    outbound: &UnboundedSender<Outbound>,
    source: &apple_cf::iosurface::IOSurface,
    capture_us: u64,
    force: bool,
    force_key: &AtomicBool,
) -> Option<usize> {
    let wider = source.width() > target_w as usize || source.height() > target_h as usize;
    if !wider {
        return emit(encoder, outbound, source, capture_us, force, force_key);
    }
    if fit.is_none() {
        match FrameFit::open(target_w, target_h) {
            Ok(opened) => {
                eprintln!(
                    "capture arrived at {}x{}; encoding {target_w}x{target_h} for the viewer",
                    source.width(),
                    source.height()
                );
                *fit = Some(opened);
            }
            Err(err) => {
                eprintln!("scale: {err}");
                force_key.store(true, Ordering::Relaxed);
                return None;
            }
        }
    }
    let Some(fit) = fit.as_mut() else {
        return None;
    };
    match fit.apply(source) {
        Ok(scaled) => emit(encoder, outbound, scaled, capture_us, force, force_key),
        Err(err) => {
            eprintln!("scale: {err}");
            force_key.store(true, Ordering::Relaxed);
            None
        }
    }
}

fn emit(
    encoder: &mut Encoder,
    outbound: &UnboundedSender<Outbound>,
    surface: &apple_cf::iosurface::IOSurface,
    capture_us: u64,
    force: bool,
    force_key: &AtomicBool,
) -> Option<usize> {
    match encoder.encode(surface, capture_us, force) {
        Ok(Some(packet)) => publish_packet(outbound, packet),
        Ok(None) => None,
        Err(err) => {
            eprintln!("encode: {err}");
            force_key.store(true, Ordering::Relaxed);
            None
        }
    }
}

fn publish_packet(outbound: &UnboundedSender<Outbound>, packet: crate::encode::EncodedPacket) -> Option<usize> {
    if packet.frame.payload.is_empty() {
        return None;
    }
    if let Some(config) = packet.config {
        let _ = outbound.send(Outbound::Control(Control::CodecConfig(config)));
    }
    let bytes = packet.frame.payload.len();
    let _ = outbound.send(Outbound::Frame(packet.frame));
    Some(bytes)
}

fn publish_cursor(
    outbound: &UnboundedSender<Outbound>,
    geometry: DisplayGeometry,
    last: &mut Option<(i32, i32, bool)>,
) {
    let Some((global_x, global_y)) = mouse_global() else {
        return;
    };
    let (x, y) = geometry.global_to_pixel(global_x, global_y);
    let visible = geometry.contains_pixel(x, y);
    if *last == Some((x, y, visible)) {
        return;
    }
    *last = Some((x, y, visible));
    let _ = outbound.send(Outbound::Control(Control::CursorPos(CursorPos {
        x,
        y,
        visible,
    })));
}
