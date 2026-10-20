use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::mpsc::UnboundedSender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deskbridge_proto::{Control, CodecId, CursorPos, Input, VideoFrame};

use crate::capture::Capture;
use crate::cg::mouse_global;
use crate::cursor::arrow_shape;
use crate::encode::Encoder;
use crate::error::PlatformError;
use crate::input::Injector;

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
    input_tx: mpsc::Sender<Input>,
    force_key: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    pub fn spawn(options: HostOptions, outbound: UnboundedSender<Outbound>) -> Result<Self, PlatformError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let force_key = Arc::new(AtomicBool::new(true));
        let stop_flag = Arc::clone(&stop);
        let force_flag = Arc::clone(&force_key);
        let thread = thread::spawn(move || {
            if let Err(err) = run(options, outbound, input_rx, ready_tx, stop_flag, force_flag) {
                eprintln!("deskbridge host: {err}");
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                input_tx,
                force_key,
                thread: Some(thread),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(PlatformError::new("capture thread stopped before it started")),
        }
    }

    pub fn inject(&self, input: Input) {
        let _ = self.input_tx.send(input);
    }

    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    options: HostOptions,
    outbound: UnboundedSender<Outbound>,
    input_rx: Receiver<Input>,
    ready: Sender<Result<(), PlatformError>>,
    stop: Arc<AtomicBool>,
    force_key: Arc<AtomicBool>,
) -> Result<(), PlatformError> {
    let capture = match Capture::start(options.display_index, options.resolution) {
        Ok(capture) => capture,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
    let mut encoder = match Encoder::open(
        options.codec,
        capture.info.width,
        capture.info.height,
        options.bitrate,
        capture.fps,
    ) {
        Ok(encoder) => encoder,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
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
        encoder.codec().name(),
        limit
    );

    let mut injector = Injector::new(capture.geometry);
    let mut last_cursor: Option<(i32, i32, bool)> = None;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(input) = input_rx.try_recv() {
            if options.inject {
                injector.apply(input);
            }
        }
        if let Some(frame) = capture.wait_frame(Duration::from_millis(8)) {
            let force = force_key.swap(false, Ordering::Relaxed);
            match encoder.encode(&frame.surface, frame.capture_us, force) {
                Ok(Some(packet)) => {
                    if let Some(config) = packet.config {
                        let _ = outbound.send(Outbound::Control(Control::CodecConfig(config)));
                    }
                    let _ = outbound.send(Outbound::Frame(packet.frame));
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("encode: {err}");
                    force_key.store(true, Ordering::Relaxed);
                }
            }
        }
        publish_cursor(&outbound, capture.geometry, &mut last_cursor);
    }
    capture.stop();
    Ok(())
}

fn publish_cursor(
    outbound: &UnboundedSender<Outbound>,
    geometry: crate::capture::DisplayGeometry,
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
