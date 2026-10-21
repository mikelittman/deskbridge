//! Screen and monitor-audio capture.
//!
//! The preferred path is one XDG remote-desktop session that can both record
//! a monitor and inject input. X11 `XGetImage` is the fallback, without audio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use deskbridge_proto::DisplayInfo;

use crate::codec::even;
use crate::error::PlatformError;

#[derive(Clone, Copy, Debug)]
pub struct DisplayGeometry {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub origin_x: f64,
    pub origin_y: f64,
    pub point_width: f64,
    pub point_height: f64,
}

impl DisplayGeometry {
    pub fn pixel_to_global(self, x: i32, y: i32) -> (f64, f64) {
        let scale_x = self.point_width / f64::from(self.pixel_width.max(1));
        let scale_y = self.point_height / f64::from(self.pixel_height.max(1));
        (
            self.origin_x + f64::from(x) * scale_x,
            self.origin_y + f64::from(y) * scale_y,
        )
    }

    pub fn global_to_pixel(self, x: f64, y: f64) -> (i32, i32) {
        let scale_x = f64::from(self.pixel_width) / self.point_width.max(1.0);
        let scale_y = f64::from(self.pixel_height) / self.point_height.max(1.0);
        (
            ((x - self.origin_x) * scale_x) as i32,
            ((y - self.origin_y) * scale_y) as i32,
        )
    }

    pub fn contains_pixel(self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.pixel_width && (y as u32) < self.pixel_height
    }
}

pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub changed: Option<f32>,
}

pub struct PcmChunk {
    pub rate: u32,
    pub channels: u8,
    pub interleaved: Vec<f32>,
}

#[derive(Debug)]
pub(crate) enum InjectEvent {
    Move(f64, f64),
    Button { button: i32, down: bool },
    Scroll { dx: i32, dy: i32 },
    Key { code: i32, down: bool },
}

#[derive(Clone)]
pub(crate) enum InjectRoute {
    Portal {
        tx: std::sync::mpsc::Sender<InjectEvent>,
        stream: u32,
    },
    X11 { screen: i32 },
}

static ROUTE: Mutex<Option<InjectRoute>> = Mutex::new(None);

pub(crate) fn inject_route() -> Option<InjectRoute> {
    ROUTE.lock().ok().and_then(|guard| guard.clone())
}

fn set_route(route: Option<InjectRoute>) {
    if let Ok(mut guard) = ROUTE.lock() {
        *guard = route;
    }
}

struct Latest {
    frame: Mutex<Option<CapturedFrame>>,
    cv: Condvar,
}

struct AudioSlot {
    pending: Mutex<PendingAudio>,
}

struct PendingAudio {
    rate: u32,
    channels: u8,
    pcm: Vec<f32>,
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    latest: Arc<Latest>,
    audio: Arc<AudioSlot>,
    pub info: DisplayInfo,
    pub geometry: DisplayGeometry,
    pub fps: u32,
}

pub fn fit_capture_size(native_w: u32, native_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let native_w = even(native_w);
    let native_h = even(native_h);
    let max_w = even(max_w);
    let max_h = even(max_h);
    if native_w <= max_w && native_h <= max_h {
        return (native_w, native_h);
    }
    let scale = (f64::from(max_w) / f64::from(native_w)).min(f64::from(max_h) / f64::from(native_h));
    let width = even((f64::from(native_w) * scale).floor() as u32).min(max_w);
    let height = even((f64::from(native_h) * scale).floor() as u32).min(max_h);
    (width.max(2), height.max(2))
}

pub(crate) fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let sw = sw.max(1);
    let sh = sh.max(1);
    let dw = dw.max(1);
    let dh = dh.max(1);
    let mut dst = vec![0u8; dw as usize * dh as usize * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
            let mut b = 0u32;
            let mut g = 0u32;
            let mut r = 0u32;
            let mut a = 0u32;
            let mut n = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * sw + sx) * 4) as usize;
                    if i + 3 >= src.len() {
                        continue;
                    }
                    b += u32::from(src[i]);
                    g += u32::from(src[i + 1]);
                    r += u32::from(src[i + 2]);
                    a += u32::from(src[i + 3]);
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let o = ((y * dw + x) * 4) as usize;
            dst[o] = (b / n) as u8;
            dst[o + 1] = (g / n) as u8;
            dst[o + 2] = (r / n) as u8;
            dst[o + 3] = (a / n) as u8;
        }
    }
    dst
}

fn sample_hash(bgra: &[u8], width: u32, height: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let step_y = (height / 16).max(1);
    let step_x = (width / 16).max(1);
    let mut y = 0u32;
    while y < height {
        let mut x = 0u32;
        while x < width {
            let i = ((y * width + x) * 4) as usize;
            if i + 2 < bgra.len() {
                hash ^= u64::from(bgra[i]);
                hash = hash.wrapping_mul(0x100_0000_01b3);
                hash ^= u64::from(bgra[i + 1]);
                hash = hash.wrapping_mul(0x100_0000_01b3);
                hash ^= u64::from(bgra[i + 2]);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
            x += step_x;
        }
        y += step_y;
    }
    hash
}

impl Capture {
    pub fn start(display_index: usize, limit: Option<(u32, u32)>) -> Result<Self, PlatformError> {
        match portal_capture(display_index, limit) {
            Ok(capture) => Ok(capture),
            Err(err) => {
                eprintln!("deskbridge: portal capture unavailable ({err}); trying X11");
                x11_capture(display_index, limit)
            }
        }
    }

    pub fn take_audio(&self) -> Option<PcmChunk> {
        let mut pending = self.audio.pending.lock().unwrap_or_else(|err| err.into_inner());
        if pending.pcm.is_empty() || pending.rate == 0 || pending.channels == 0 {
            return None;
        }
        Some(PcmChunk {
            rate: pending.rate,
            channels: pending.channels,
            interleaved: std::mem::take(&mut pending.pcm),
        })
    }

    pub fn wait_frame(&self, timeout: Duration) -> Option<CapturedFrame> {
        let mut guard = self.latest.frame.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(frame) = guard.take() {
            return Some(frame);
        }
        let (mut guard, _) = self
            .latest
            .cv
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|err| err.into_inner());
        guard.take()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.latest.cv.notify_all();
        if let Some(thread) = self.thread.lock().unwrap_or_else(|err| err.into_inner()).take() {
            let _ = thread.join();
        }
        set_route(None);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn publish_frame(latest: &Latest, bgra: Vec<u8>, width: u32, height: u32, previous: &mut Option<u64>) {
    let hash = sample_hash(&bgra, width, height);
    let changed = Some(if *previous == Some(hash) { 0.0 } else { 1.0 });
    *previous = Some(hash);
    let mut slot = latest.frame.lock().unwrap_or_else(|err| err.into_inner());
    *slot = Some(CapturedFrame { bgra, width, height, changed });
    latest.cv.notify_one();
}

fn push_pcm(audio: &AudioSlot, rate: u32, channels: u8, samples: &[f32]) {
    if rate == 0 || channels == 0 || samples.is_empty() {
        return;
    }
    let mut pending = audio.pending.lock().unwrap_or_else(|err| err.into_inner());
    if pending.rate != rate || pending.channels != channels {
        pending.pcm.clear();
        pending.rate = rate;
        pending.channels = channels;
    }
    pending.pcm.extend_from_slice(samples);
    let cap = rate as usize * usize::from(channels) / 5;
    if cap > 0 && pending.pcm.len() > cap {
        let drop_n = pending.pcm.len() - cap;
        pending.pcm.drain(..drop_n);
    }
}

struct Started {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    latest: Arc<Latest>,
    audio: Arc<AudioSlot>,
    info: DisplayInfo,
    geometry: DisplayGeometry,
    fps: u32,
}

fn finish(started: Started) -> Capture {
    Capture {
        stop: started.stop,
        thread: Mutex::new(Some(started.thread)),
        latest: started.latest,
        audio: started.audio,
        info: started.info,
        geometry: started.geometry,
        fps: started.fps,
    }
}

fn portal_capture(display_index: usize, limit: Option<(u32, u32)>) -> Result<Capture, PlatformError> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread = thread::spawn(move || portal_thread(display_index, limit, stop_thread, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(started)) => Ok(finish(Started {
            stop: started.stop,
            thread,
            latest: started.latest,
            audio: started.audio,
            info: started.info,
            geometry: started.geometry,
            fps: started.fps,
        })),
        Ok(Err(err)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(err)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(PlatformError::new("portal capture stopped before it started"))
        }
    }
}

fn portal_thread(
    display_index: usize,
    limit: Option<(u32, u32)>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<StartedParts, PlatformError>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = ready_tx.send(Err(PlatformError::new(err)));
            return;
        }
    };
    if let Err(err) = runtime.block_on(portal_run(display_index, limit, Arc::clone(&stop), ready_tx)) {
        // portal_run sends the error itself when setup fails before ready.
        let _ = err;
    }
}

struct StartedParts {
    stop: Arc<AtomicBool>,
    latest: Arc<Latest>,
    audio: Arc<AudioSlot>,
    info: DisplayInfo,
    geometry: DisplayGeometry,
    fps: u32,
}

async fn portal_run(
    display_index: usize,
    limit: Option<(u32, u32)>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<StartedParts, PlatformError>>,
) -> Result<(), PlatformError> {
    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    let remote = RemoteDesktop::new().await.map_err(PlatformError::new)?;
    let session = remote.create_session().await.map_err(PlatformError::new)?;
    remote
        .select_devices(
            &session,
            DeviceType::Keyboard | DeviceType::Pointer,
            None,
            PersistMode::DoNot,
        )
        .await
        .map_err(PlatformError::new)?
        .response()
        .map_err(PlatformError::new)?;
    let cast = Screencast::new().await.map_err(PlatformError::new)?;
    cast.select_sources(
        &session,
        CursorMode::Hidden,
        BitFlags::from(SourceType::Monitor),
        true,
        None,
        PersistMode::DoNot,
    )
    .await
    .map_err(PlatformError::new)?
    .response()
    .map_err(PlatformError::new)?;
    let selected = remote
        .start(&session, None)
        .await
        .map_err(PlatformError::new)?
        .response()
        .map_err(PlatformError::new)?;
    let streams = selected.streams().unwrap_or(&[]);
    let stream = streams.get(display_index).ok_or_else(|| {
        PlatformError::new(format!(
            "display {display_index} is not shared ({count} stream(s))",
            count = streams.len()
        ))
    })?;
    let node = stream.pipe_wire_node_id();
    // Absolute pointer motion is in the stream's own coordinate space.
    let logical = stream.size().map(|(w, h)| (w.max(2) as u32, h.max(2) as u32));
    let fd = cast.open_pipe_wire_remote(&session).await.map_err(PlatformError::new)?;
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<InjectEvent>();
    set_route(Some(InjectRoute::Portal { tx: cmd_tx, stream: node }));

    let latest = Arc::new(Latest { frame: Mutex::new(None), cv: Condvar::new() });
    let audio = Arc::new(AudioSlot {
        pending: Mutex::new(PendingAudio { rate: 0, channels: 0, pcm: Vec::new() }),
    });
    let (info_tx, info_rx) = std::sync::mpsc::channel();
    let stop_video = Arc::clone(&stop);
    let latest_video = Arc::clone(&latest);
    let video_thread = thread::spawn(move || pipewire_video(fd, node, limit, logical, latest_video, stop_video, info_tx));
    let described = match info_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(value) => value,
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = video_thread.join();
            let err = PlatformError::new("PipeWire did not produce a frame");
            let _ = ready_tx.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
    let (info, geometry, fps) = match described {
        Ok(value) => value,
        Err(err) => {
            stop.store(true, Ordering::Relaxed);
            let _ = video_thread.join();
            let _ = ready_tx.send(Err(PlatformError::new(err.to_string())));
            return Err(err);
        }
    };
    let stop_audio = Arc::clone(&stop);
    let audio_slot = Arc::clone(&audio);
    let audio_thread = thread::spawn(move || {
        if let Err(err) = pipewire_audio(audio_slot, stop_audio) {
            eprintln!("deskbridge: monitor audio unavailable ({err})");
        }
    });
    if ready_tx
        .send(Ok(StartedParts {
            stop: Arc::clone(&stop),
            latest,
            audio,
            info,
            geometry,
            fps,
        }))
        .is_err()
    {
        stop.store(true, Ordering::Relaxed);
    }
    while !stop.load(Ordering::Relaxed) {
        while let Ok(event) = cmd_rx.try_recv() {
            apply_portal(&remote, &session, node, event).await;
        }
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    let _ = session.close().await;
    let _ = video_thread.join();
    let _ = audio_thread.join();
    Ok(())
}

async fn apply_portal(
    remote: &ashpd::desktop::remote_desktop::RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, ashpd::desktop::remote_desktop::RemoteDesktop<'_>>,
    node: u32,
    event: InjectEvent,
) {
    use ashpd::desktop::remote_desktop::{Axis, KeyState};
    let result = match event {
        InjectEvent::Move(x, y) => remote.notify_pointer_motion_absolute(session, node, x, y).await,
        InjectEvent::Button { button, down } => {
            let state = if down { KeyState::Pressed } else { KeyState::Released };
            remote.notify_pointer_button(session, button, state).await
        }
        InjectEvent::Scroll { dx, dy } => {
            if dy != 0 {
                let _ = remote.notify_pointer_axis_discrete(session, Axis::Vertical, dy).await;
            }
            if dx != 0 {
                let _ = remote.notify_pointer_axis_discrete(session, Axis::Horizontal, dx).await;
            }
            Ok(())
        }
        InjectEvent::Key { code, down } => {
            let state = if down { KeyState::Pressed } else { KeyState::Released };
            remote.notify_keyboard_keycode(session, code, state).await
        }
    };
    if let Err(err) = result {
        static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::Relaxed) {
            eprintln!("deskbridge: input injection failed ({err})");
        }
    }
}

fn pipewire_video(
    fd: std::os::fd::OwnedFd,
    node: u32,
    limit: Option<(u32, u32)>,
    logical: Option<(u32, u32)>,
    latest: Arc<Latest>,
    stop: Arc<AtomicBool>,
    info_tx: std::sync::mpsc::Sender<Result<(DisplayInfo, DisplayGeometry, u32), PlatformError>>,
) {
    let thread_loop = match unsafe { pipewire::thread_loop::ThreadLoop::new(Some("deskbridge-video"), None) } {
        Ok(loop_) => loop_,
        Err(err) => {
            let _ = info_tx.send(Err(PlatformError::new(err)));
            return;
        }
    };
    let context = match pipewire::context::Context::new(&thread_loop) {
        Ok(context) => context,
        Err(err) => {
            let _ = info_tx.send(Err(PlatformError::new(err)));
            return;
        }
    };
    let core = match context.connect_fd(fd, None) {
        Ok(core) => core,
        Err(err) => {
            let _ = info_tx.send(Err(PlatformError::new(err)));
            return;
        }
    };
    let props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Video",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Screen",
    };
    let stream = match pipewire::stream::Stream::new(&core, "deskbridge-screen", props) {
        Ok(stream) => stream,
        Err(err) => {
            let _ = info_tx.send(Err(PlatformError::new(err)));
            return;
        }
    };
    let user = VideoUser {
        format: pipewire::spa::param::video::VideoInfoRaw::new(),
        latest,
        limit,
        logical,
        previous: None,
        announced: false,
        info_tx: Some(info_tx),
    };
    let listener = match stream
        .add_local_listener_with_user_data(user)
        .param_changed(|_, user, id, param| {
            let Some(param) = param else { return };
            if id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media, sub)) = pipewire::spa::param::format_utils::parse_format(param) else { return };
            if media != pipewire::spa::param::format::MediaType::Video
                || sub != pipewire::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if user.format.parse(param).is_err() {
                return;
            }
        })
        .process(|stream, user| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let width = user.format.size().width.max(2);
            let height = user.format.size().height.max(2);
            if width < 2 || height < 2 {
                return;
            }
            let Some(bgra) = packed_to_bgra(&mut datas[0], width, height, user.format.format()) else { return };
            let (out_w, out_h, pixels) = fit_frame(&bgra, width, height, user.limit);
            if !user.announced {
                let fps = frame_rate(&user.format);
                let point_w = user.logical.map(|(w, _)| w).unwrap_or(width);
                let point_h = user.logical.map(|(_, h)| h).unwrap_or(height);
                let info = DisplayInfo {
                    width: out_w,
                    height: out_h,
                    refresh_millihz: fps.saturating_mul(1000),
                };
                let geometry = DisplayGeometry {
                    pixel_width: out_w,
                    pixel_height: out_h,
                    origin_x: 0.0,
                    origin_y: 0.0,
                    point_width: f64::from(point_w.max(1)),
                    point_height: f64::from(point_h.max(1)),
                };
                if let Some(tx) = user.info_tx.take() {
                    let _ = tx.send(Ok((info, geometry, fps)));
                }
                user.announced = true;
            }
            publish_frame(&user.latest, pixels, out_w, out_h, &mut user.previous);
        })
        .register()
    {
        Ok(listener) => listener,
        Err(err) => {
            let _ = err;
            return;
        }
    };
    let pod = match video_pod() {
        Ok(pod) => pod,
        Err(err) => {
            let _ = err;
            return;
        }
    };
    let Some(param) = pipewire::spa::pod::Pod::from_bytes(&pod) else {
        return;
    };
    let mut params = [param];
    if let Err(err) = stream.connect(
        pipewire::spa::utils::Direction::Input,
        Some(node),
        pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        let _ = err;
        return;
    }
    thread_loop.start();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(20));
    }
    thread_loop.stop();
    drop(listener);
    drop(stream);
}

struct VideoUser {
    format: pipewire::spa::param::video::VideoInfoRaw,
    latest: Arc<Latest>,
    limit: Option<(u32, u32)>,
    logical: Option<(u32, u32)>,
    previous: Option<u64>,
    announced: bool,
    info_tx: Option<std::sync::mpsc::Sender<Result<(DisplayInfo, DisplayGeometry, u32), PlatformError>>>,
}

impl Drop for VideoUser {
    fn drop(&mut self) {
        if self.announced {
            return;
        }
        if let Some(tx) = self.info_tx.take() {
            let _ = tx.send(Err(PlatformError::new("PipeWire video stream stopped")));
        }
    }
}

fn frame_rate(format: &pipewire::spa::param::video::VideoInfoRaw) -> u32 {
    let rate = format.framerate();
    let fps = if rate.denom == 0 { rate.num } else { rate.num / rate.denom.max(1) };
    if fps == 0 { 60 } else { fps.clamp(1, 120) }
}

fn fit_frame(bgra: &[u8], width: u32, height: u32, limit: Option<(u32, u32)>) -> (u32, u32, Vec<u8>) {
    let (width, height) = (even(width), even(height));
    let Some((max_w, max_h)) = limit else {
        return (width, height, bgra.to_vec());
    };
    let (dw, dh) = fit_capture_size(width, height, max_w, max_h);
    if dw == width && dh == height {
        (width, height, bgra.to_vec())
    } else {
        (dw, dh, scale_bgra(bgra, width, height, dw, dh))
    }
}

fn packed_to_bgra(
    data: &mut pipewire::spa::buffer::Data,
    width: u32,
    height: u32,
    format: pipewire::spa::param::video::VideoFormat,
) -> Option<Vec<u8>> {
    use pipewire::spa::param::video::VideoFormat;
    let offset = data.chunk().offset() as usize;
    let chunk_stride = data.chunk().stride();
    let bytes = data.data()?;
    if offset >= bytes.len() {
        return None;
    }
    let plane = &bytes[offset..];
    let bpp = match format {
        VideoFormat::BGRA | VideoFormat::BGRx | VideoFormat::RGBA | VideoFormat::RGBx => 4,
        VideoFormat::BGR | VideoFormat::RGB => 3,
        _ => return None,
    };
    let stride = if chunk_stride > 0 { chunk_stride as usize } else { width as usize * bpp };
    let stride = if stride >= width as usize * bpp { stride } else { width as usize * bpp };
    let mut out = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        let row = y * stride;
        for x in 0..width as usize {
            let src = row + x * bpp;
            let dst = (y * width as usize + x) * 4;
            if src + bpp > plane.len() || dst + 3 >= out.len() {
                continue;
            }
            let (b, g, r, a) = match format {
                VideoFormat::BGRA => (plane[src], plane[src + 1], plane[src + 2], plane[src + 3]),
                VideoFormat::BGRx => (plane[src], plane[src + 1], plane[src + 2], 255),
                VideoFormat::RGBA => (plane[src + 2], plane[src + 1], plane[src], plane[src + 3]),
                VideoFormat::RGBx => (plane[src + 2], plane[src + 1], plane[src], 255),
                VideoFormat::BGR => (plane[src], plane[src + 1], plane[src + 2], 255),
                VideoFormat::RGB => (plane[src + 2], plane[src + 1], plane[src], 255),
                _ => (0, 0, 0, 0),
            };
            out[dst] = b;
            out[dst + 1] = g;
            out[dst + 2] = r;
            out[dst + 3] = a;
        }
    }
    Some(out)
}

fn video_pod() -> Result<Vec<u8>, PlatformError> {
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::utils::{Fraction, Rectangle};
    let obj = pipewire::spa::pod::object!(
        pipewire::spa::utils::SpaTypes::ObjectParamFormat,
        pipewire::spa::param::ParamType::EnumFormat,
        pipewire::spa::pod::property!(
            pipewire::spa::param::format::FormatProperties::MediaType,
            Id,
            pipewire::spa::param::format::MediaType::Video
        ),
        pipewire::spa::pod::property!(
            pipewire::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pipewire::spa::param::format::MediaSubtype::Raw
        ),
        pipewire::spa::pod::property!(
            pipewire::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pipewire::spa::param::video::VideoFormat::BGRx,
            pipewire::spa::param::video::VideoFormat::BGRA,
            pipewire::spa::param::video::VideoFormat::RGBx,
            pipewire::spa::param::video::VideoFormat::RGBA,
            pipewire::spa::param::video::VideoFormat::BGR,
            pipewire::spa::param::video::VideoFormat::RGB
        ),
        pipewire::spa::pod::property!(
            pipewire::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        pipewire::spa::pod::property!(
            pipewire::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pipewire::spa::pod::Value::Object(obj))
        .map(|cursor| cursor.0.into_inner())
        .map_err(|err| PlatformError::new(err))
}

fn pipewire_audio(audio: Arc<AudioSlot>, stop: Arc<AtomicBool>) -> Result<(), PlatformError> {
    let thread_loop = unsafe { pipewire::thread_loop::ThreadLoop::new(Some("deskbridge-audio"), None) }
        .map_err(PlatformError::new)?;
    let context = pipewire::context::Context::new(&thread_loop).map_err(PlatformError::new)?;
    let core = context.connect(None).map_err(PlatformError::new)?;
    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
    };
    props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");
    let stream = pipewire::stream::Stream::new(&core, "deskbridge-audio", props).map_err(PlatformError::new)?;
    let user = AudioUser { format: pipewire::spa::param::audio::AudioInfoRaw::new(), audio };
    let listener = stream
        .add_local_listener_with_user_data(user)
        .param_changed(|_, user, id, param| {
            let Some(param) = param else { return };
            if id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let _ = user.format.parse(param);
        })
        .process(|stream, user| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let offset = datas[0].chunk().offset() as usize;
            let size = datas[0].chunk().size() as usize;
            let Some(bytes) = datas[0].data() else { return };
            if offset >= bytes.len() {
                return;
            }
            let end = (offset + size).min(bytes.len());
            let samples = &bytes[offset..end];
            let channels = user.format.channels().clamp(1, 8) as u8;
            let rate = user.format.rate().max(8_000);
            let floats = f32_from_bytes(samples, channels);
            push_pcm(&user.audio, rate, channels.min(2), &floats);
        })
        .register()
        .map_err(PlatformError::new)?;
    let mut info = pipewire::spa::param::audio::AudioInfoRaw::new();
    info.set_format(pipewire::spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let obj = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(obj),
    )
    .map_err(PlatformError::new)?
    .0
    .into_inner();
    let mut params = [pipewire::spa::pod::Pod::from_bytes(&bytes).ok_or_else(|| PlatformError::new("audio format"))?];
    stream
        .connect(
            pipewire::spa::utils::Direction::Input,
            None,
            pipewire::stream::StreamFlags::AUTOCONNECT
                | pipewire::stream::StreamFlags::MAP_BUFFERS
                | pipewire::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(PlatformError::new)?;
    thread_loop.start();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(30));
    }
    thread_loop.stop();
    drop(listener);
    Ok(())
}

struct AudioUser {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    audio: Arc<AudioSlot>,
}

fn f32_from_bytes(bytes: &[u8], channels: u8) -> Vec<f32> {
    let channels = usize::from(channels.max(1));
    let mut out = Vec::new();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let frame_end = index + channels * 4;
        if frame_end > bytes.len() {
            break;
        }
        let take = channels.min(2);
        for channel in 0..take {
            let start = index + channel * 4;
            let sample = f32::from_le_bytes(bytes[start..start + 4].try_into().unwrap_or([0; 4]));
            out.push(sample);
        }
        index += channels * 4;
    }
    out
}

fn x11_capture(display_index: usize, limit: Option<(u32, u32)>) -> Result<Capture, PlatformError> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread = thread::spawn(move || x11_thread(display_index, limit, stop_thread, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(parts)) => Ok(finish(Started {
            stop: parts.stop,
            thread,
            latest: parts.latest,
            audio: parts.audio,
            info: parts.info,
            geometry: parts.geometry,
            fps: parts.fps,
        })),
        Ok(Err(err)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(err)
        }
        Err(_) => Err(PlatformError::new("X11 capture stopped before it started")),
    }
}

fn x11_thread(
    display_index: usize,
    limit: Option<(u32, u32)>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<StartedParts, PlatformError>>,
) {
    let x11 = match X11::open() {
        Ok(x11) => x11,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    let screen = display_index as i32;
    let native_w = unsafe { (x11.display_width)(x11.display, screen) };
    let native_h = unsafe { (x11.display_height)(x11.display, screen) };
    if native_w <= 1 || native_h <= 1 {
        let _ = ready_tx.send(Err(PlatformError::new(format!(
            "display {display_index} is not connected"
        ))));
        return;
    }
    let root = unsafe { (x11.root_window)(x11.display, screen) };
    let (out_w, out_h) = match limit {
        Some((max_w, max_h)) => fit_capture_size(native_w as u32, native_h as u32, max_w, max_h),
        None => (even(native_w as u32), even(native_h as u32)),
    };
    let fps = 60u32;
    let info = DisplayInfo { width: out_w, height: out_h, refresh_millihz: 60_000 };
    let geometry = DisplayGeometry {
        pixel_width: out_w,
        pixel_height: out_h,
        origin_x: 0.0,
        origin_y: 0.0,
        point_width: f64::from(native_w as u32),
        point_height: f64::from(native_h as u32),
    };
    set_route(Some(InjectRoute::X11 { screen }));
    let latest = Arc::new(Latest { frame: Mutex::new(None), cv: Condvar::new() });
    let audio = Arc::new(AudioSlot {
        pending: Mutex::new(PendingAudio { rate: 0, channels: 0, pcm: Vec::new() }),
    });
    let parts = StartedParts {
        stop: Arc::clone(&stop),
        latest: Arc::clone(&latest),
        audio,
        info,
        geometry,
        fps,
    };
    if ready_tx.send(Ok(parts)).is_err() {
        return;
    }
    let mut previous = None;
    let interval = Duration::from_millis(1000 / u64::from(fps));
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if let Some((bgra, width, height)) = x11.grab(root, native_w as u32, native_h as u32) {
            let (w, h, pixels) = fit_frame(&bgra, width, height, limit);
            publish_frame(&latest, pixels, w, h, &mut previous);
        }
        let spent = started.elapsed();
        if spent < interval {
            thread::sleep(interval - spent);
        }
    }
    unsafe { (x11.close_display)(x11.display) };
}

struct X11 {
    _lib: libloading::Library,
    display: *mut std::ffi::c_void,
    display_width: unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32,
    display_height: unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32,
    root_window: unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> u64,
    get_image: unsafe extern "C" fn(*mut std::ffi::c_void, u64, i32, i32, u32, u32, u64, i32) -> *mut XImage,
    destroy_image: unsafe extern "C" fn(*mut XImage) -> i32,
    close_display: unsafe extern "C" fn(*mut std::ffi::c_void) -> i32,
}

#[repr(C)]
struct XImage {
    width: i32,
    height: i32,
    xoffset: i32,
    format: i32,
    data: *mut u8,
    byte_order: i32,
    bitmap_unit: i32,
    bitmap_bit_order: i32,
    bitmap_pad: i32,
    depth: i32,
    bytes_per_line: i32,
    bits_per_pixel: i32,
    red_mask: u64,
    green_mask: u64,
    blue_mask: u64,
}

impl X11 {
    fn open() -> Result<Self, PlatformError> {
        let lib = unsafe { libloading::Library::new("libX11.so.6") }
            .map_err(|_| PlatformError::new("libX11.so.6 is not installed"))?;
        unsafe fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T, PlatformError> {
            let symbol: libloading::Symbol<T> = unsafe { lib.get(name) }
                .map_err(|_| PlatformError::new(format!("missing {}", String::from_utf8_lossy(name))))?;
            Ok(*symbol)
        }
        let init: unsafe extern "C" fn() -> i32 = unsafe { sym(&lib, b"XInitThreads\0") }?;
        unsafe { init() };
        let open: unsafe extern "C" fn(*const i8) -> *mut std::ffi::c_void = unsafe { sym(&lib, b"XOpenDisplay\0") }?;
        let display = unsafe { open(std::ptr::null()) };
        if display.is_null() {
            return Err(PlatformError::new(
                "XOpenDisplay failed. Start the host from a logged-in desktop session.",
            ));
        }
        Ok(Self {
            display_width: unsafe { sym(&lib, b"XDisplayWidth\0") }?,
            display_height: unsafe { sym(&lib, b"XDisplayHeight\0") }?,
            root_window: unsafe { sym(&lib, b"XRootWindow\0") }?,
            get_image: unsafe { sym(&lib, b"XGetImage\0") }?,
            destroy_image: unsafe { sym(&lib, b"XDestroyImage\0") }?,
            close_display: unsafe { sym(&lib, b"XCloseDisplay\0") }?,
            _lib: lib,
            display,
        })
    }

    fn grab(&self, root: u64, width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
        let image = unsafe { (self.get_image)(self.display, root, 0, 0, width, height, !0, 2) };
        if image.is_null() {
            return None;
        }
        let grabbed = unsafe {
            let image_ref = &*image;
            let bgra = ximage_bgra(image_ref);
            (self.destroy_image)(image);
            bgra
        };
        let (pixels, w, h) = grabbed?;
        Some((pixels, w, h))
    }
}

fn ximage_bgra(image: &XImage) -> Option<(Vec<u8>, u32, u32)> {
    if image.data.is_null() || image.width <= 0 || image.height <= 0 || image.bits_per_pixel < 24 {
        return None;
    }
    let width = image.width as u32;
    let height = image.height as u32;
    let bpp = (image.bits_per_pixel / 8) as usize;
    let stride = image.bytes_per_line.max(0) as usize;
    let mut out = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = y * stride + x * bpp;
            let pixel = unsafe {
                let ptr = image.data.add(src);
                if image.byte_order == 0 {
                    u32::from(ptr.read())
                        | (u32::from(ptr.add(1).read()) << 8)
                        | (u32::from(ptr.add(2).read()) << 16)
                        | if bpp > 3 { u32::from(ptr.add(3).read()) << 24 } else { 0 }
                } else {
                    let mut value = 0u32;
                    for byte in 0..bpp.min(4) {
                        value = (value << 8) | u32::from(ptr.add(byte).read());
                    }
                    value
                }
            };
            let dst = (y * width as usize + x) * 4;
            out[dst] = channel(pixel, image.blue_mask);
            out[dst + 1] = channel(pixel, image.green_mask);
            out[dst + 2] = channel(pixel, image.red_mask);
            out[dst + 3] = 255;
        }
    }
    Some((out, even(width), even(height)))
}

fn channel(pixel: u32, mask: u64) -> u8 {
    let mask = mask as u32;
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones();
    let value = (pixel & mask) >> shift;
    if bits >= 8 {
        (value >> (bits - 8)) as u8
    } else {
        (value << (8 - bits)) as u8
    }
}
