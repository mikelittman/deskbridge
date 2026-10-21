use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use deskbridge_proto::{AudioPacket, CodecId, Control, Input, VideoFrame};
use deskbridge_session::pace::{encode_fps, picture_size, FramePace};
use deskbridge_session::{mono_us, LinkHealth};

use crate::audio::AudioEncoder;
use crate::capture::{fit_capture_size, scale_bgra, Capture, CapturedFrame, DisplayGeometry};
use crate::encode::Encoder;
use crate::error::PlatformError;
use crate::input::Injector;
use crate::present::arrow_shape;

const PREVIEW_W: u32 = 640;
const PREVIEW_H: u32 = 360;

pub struct HostOptions {
    pub display_index: usize,
    pub bitrate: u32,
    pub inject: bool,
    pub codec: CodecId,
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
        drop(self.input_tx.take());
        if let Some(thread) = self.input_thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn prepare(inject: bool) -> Result<(), PlatformError> {
    let _ = inject;
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
    {
        Ok(())
    } else {
        Err(PlatformError::new(
            "start the host from a logged-in desktop session (WAYLAND_DISPLAY, DISPLAY, or DBUS_SESSION_BUS_ADDRESS is not set)",
        ))
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
    let preview = options.resolution.is_none()
        && fit_capture_size(capture.info.width, capture.info.height, PREVIEW_W, PREVIEW_H)
            != (capture.info.width, capture.info.height);
    let (open_w, open_h) = if preview {
        fit_capture_size(capture.info.width, capture.info.height, PREVIEW_W, PREVIEW_H)
    } else {
        (capture.info.width, capture.info.height)
    };
    let mut encoder = match Encoder::open(options.codec, open_w, open_h, link.pace_bps(), capture.fps) {
        Ok(encoder) => encoder,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::new(err.to_string())));
            capture.stop();
            return Err(err);
        }
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
            "60 Hz".to_string()
        } else {
            format!("{:.0} Hz", f64::from(capture.info.refresh_millihz) / 1000.0)
        },
        options.codec.name(),
        limit
    );
    if preview {
        eprintln!(
            "first picture is {open_w}x{open_h} so it can arrive quickly, then the full frame follows"
        );
    }

    let mut pace = FramePace::new(options.bitrate, capture.fps);
    let mut shown_pace = options.bitrate;
    let mut audio_encoder: Option<AudioEncoder> = None;
    let mut audio_broken = false;
    let mut latest: Option<CapturedFrame> = None;
    let mut pending: Option<CapturedFrame> = None;
    let mut preview_left = preview;
    let mut spatial = 0u8;
    let mut target_w = if preview { capture.info.width } else { open_w };
    let mut target_h = if preview { capture.info.height } else { open_h };
    let mut encoder_failed = false;
    while !stop.load(Ordering::Relaxed) {
        if !preview_left {
            let wanted = link.spatial();
            if wanted != spatial {
                spatial = wanted;
                let (width, height) = picture_size(capture.info.width, capture.info.height, spatial);
                if width != target_w || height != target_h {
                    target_w = width;
                    target_h = height;
                    force_key.store(true, Ordering::Relaxed);
                    eprintln!("picture {width}x{height} to match the link");
                }
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
        if encoder_failed {
            drain_audio(&outbound, &capture, &mut audio_encoder, &mut audio_broken);
            continue;
        }
        let due = pace.due(now) || force_key.load(Ordering::Relaxed) || preview_left;
        if pending.is_some() && !link_busy && due {
            let frame = pending.take().expect("pending frame");
            let changed = frame.changed;
            if preview_left {
                let (pw, ph) = fit_capture_size(frame.width, frame.height, PREVIEW_W, PREVIEW_H);
                if let Some(bytes) = emit(&mut encoder, &frame, pw, ph, capture_us_of(&frame), true, &outbound, &force_key) {
                    pace.observe(now, bytes, changed);
                    preview_left = false;
                    target_w = picture_size(capture.info.width, capture.info.height, spatial).0;
                    target_h = picture_size(capture.info.width, capture.info.height, spatial).1;
                    force_key.store(true, Ordering::Relaxed);
                }
            }
            let (tw, th) = if preview_left {
                fit_capture_size(frame.width, frame.height, PREVIEW_W, PREVIEW_H)
            } else {
                (target_w, target_h)
            };
            let force = force_key.swap(false, Ordering::Relaxed) || preview_left;
            encoder.set_target_fps(encode_fps(capture.fps, changed, pace_bps, options.bitrate));
            encoder.set_bitrate(pace_bps);
            match emit(&mut encoder, &frame, tw, th, capture_us_of(&frame), force, &outbound, &force_key) {
                Some(bytes) => {
                    pace.observe(now, bytes, changed);
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
        } else if pending.is_none() && !link_busy && force_key.load(Ordering::Relaxed) {
            if let Some(frame) = latest.clone_frame() {
                let changed = frame.changed;
                let force = force_key.swap(false, Ordering::Relaxed);
                if force {
                    encoder.set_target_fps(encode_fps(capture.fps, changed, pace_bps, options.bitrate));
                    encoder.set_bitrate(pace_bps);
                    match emit(&mut encoder, &frame, target_w, target_h, mono_us(), true, &outbound, &force_key) {
                        Some(bytes) => pace.observe(now, bytes, changed),
                        None => {
                            force_key.store(true, Ordering::Relaxed);
                            encoder_failed = false;
                        }
                    }
                }
            }
        }
        drain_audio(&outbound, &capture, &mut audio_encoder, &mut audio_broken);
        if encoder_open_failed(&mut encoder) {
            encoder_failed = true;
        }
    }
    capture.stop();
    Ok(())
}

fn encoder_open_failed(_encoder: &mut Encoder) -> bool {
    false
}

trait CloneFrame {
    fn clone_frame(&self) -> Option<CapturedFrame>;
}

impl CloneFrame for Option<CapturedFrame> {
    fn clone_frame(&self) -> Option<CapturedFrame> {
        self.as_ref().map(|frame| CapturedFrame {
            bgra: frame.bgra.clone(),
            width: frame.width,
            height: frame.height,
            changed: frame.changed,
        })
    }
}

fn capture_us_of(_frame: &CapturedFrame) -> u64 {
    mono_us()
}

fn emit(
    encoder: &mut Encoder,
    frame: &CapturedFrame,
    width: u32,
    height: u32,
    capture_us: u64,
    force: bool,
    outbound: &UnboundedSender<Outbound>,
    force_key: &AtomicBool,
) -> Option<usize> {
    let owned;
    let pixels = if frame.width == width && frame.height == height {
        frame.bgra.as_slice()
    } else {
        owned = scale_bgra(&frame.bgra, frame.width, frame.height, width, height);
        owned.as_slice()
    };
    match encoder.encode(pixels, width, height, capture_us, force) {
        Ok(Some(packet)) => {
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
        Ok(None) => None,
        Err(err) => {
            eprintln!("encode: {err}");
            force_key.store(true, Ordering::Relaxed);
            None
        }
    }
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
        match AudioEncoder::open(chunk.rate, chunk.channels) {
            Ok(opened) => {
                eprintln!("system audio {} Hz, {} channel(s)", chunk.rate, chunk.channels);
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
