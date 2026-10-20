use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use deskbridge_proto::{CodecId, Control, CursorPos, Input, VideoFrame};
use deskbridge_session::mono_us;

use crate::capture::{Capture, CapturedFrame, DisplayGeometry};
use crate::cg::mouse_global;
use crate::cursor::arrow_shape;
use crate::encode::Encoder;
use crate::error::PlatformError;
use crate::input::Injector;
use crate::pace::{encode_fps, FramePace};
use crate::preview::{needs_preview, Preview};

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
}

pub struct Host {
    stop: Arc<AtomicBool>,
    input_tx: Option<Sender<Input>>,
    force_key: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    input_thread: Option<JoinHandle<()>>,
    video_backlog: Arc<AtomicUsize>,
    viewer_count: Arc<AtomicUsize>,
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
        let stop_flag = Arc::clone(&stop);
        let force_flag = Arc::clone(&force_key);
        let backlog_flag = Arc::clone(&video_backlog);
        let viewers_flag = Arc::clone(&viewer_count);
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

    /// Frames accepted by viewers and not yet written to completion.
    pub fn video_backlog(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.video_backlog)
    }

    pub fn set_viewer_count(&self, count: usize) {
        self.viewer_count.store(count.max(1), Ordering::Relaxed);
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
) -> Result<(), PlatformError> {
    let capture = match Capture::start(options.display_index, options.resolution) {
        Ok(capture) => capture,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
    let mut preview = if needs_preview(capture.info.width, capture.info.height) {
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
            options.bitrate,
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
    let mut last_cursor: Option<(i32, i32, bool)> = None;
    // Kept so a viewer who joins a still screen can be sent one keyframe.
    // After that, a still screen sends nothing until the picture changes.
    let mut latest: Option<CapturedFrame> = None;
    let mut pending: Option<CapturedFrame> = None;
    let mut idle_encode_failed = false;
    let mut encoder_failed = false;
    while !stop.load(Ordering::Relaxed) {
        if let Some(frame) = capture.wait_frame(Duration::from_millis(8)) {
            let unchanged = matches!(frame.changed, Some(fraction) if fraction <= 0.0);
            if !unchanged || (latest.is_none() && pending.is_none()) {
                pending = Some(frame);
            }
        }

        let now = Instant::now();
        let viewers = viewer_count.load(Ordering::Relaxed).max(1);
        let link_busy = video_backlog.load(Ordering::Relaxed) >= viewers;
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
                capture.info.width,
                capture.info.height,
                options.bitrate,
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
                    encoder.set_target_fps(encode_fps(capture.fps, changed));
                    match emit(
                        encoder,
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
                            encoder.set_target_fps(encode_fps(capture.fps, changed));
                            match emit(
                                encoder,
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
    }
    capture.stop();
    Ok(())
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
