use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use apple_cf::cm::CMTime;
use apple_cf::iosurface::IOSurface;
use deskbridge_proto::DisplayInfo;
use deskbridge_session::mono_us;
use screencapturekit::cm::CMSampleBufferSCExt;
use screencapturekit::prelude::*;
use screencapturekit::SCFrameStatus;

use crate::cg::refresh_millihz;
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
    pub surface: IOSurface,
    pub capture_us: u64,
    /// Share of the frame that changed, from 0 to 1.
    /// `None` means ScreenCaptureKit did not say which region moved.
    pub changed: Option<f32>,
}

struct Latest {
    frame: Mutex<Option<CapturedFrame>>,
    cv: Condvar,
    width: u32,
    height: u32,
}

pub struct PcmChunk {
    pub rate: u32,
    pub channels: u8,
    pub interleaved: Vec<f32>,
}

struct PendingAudio {
    rate: u32,
    channels: u8,
    pcm: Vec<f32>,
}

struct AudioSlot {
    pending: Mutex<PendingAudio>,
    logged: AtomicBool,
}

pub struct Capture {
    stream: SCStream,
    latest: Arc<Latest>,
    audio: Arc<AudioSlot>,
    pub info: DisplayInfo,
    pub geometry: DisplayGeometry,
    pub fps: u32,
}

/// Fit a capture inside `max_w`×`max_h`. Aspect is kept, nothing is scaled up, and both sides stay even.
pub fn fit_capture_size(native_w: u32, native_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let native_w = even(native_w);
    let native_h = even(native_h);
    let max_w = even(max_w);
    let max_h = even(max_h);
    if native_w <= max_w && native_h <= max_h {
        return (native_w, native_h);
    }
    let scale =
        (f64::from(max_w) / f64::from(native_w)).min(f64::from(max_h) / f64::from(native_h));
    let width = even((f64::from(native_w) * scale).floor() as u32).min(max_w);
    let height = even((f64::from(native_h) * scale).floor() as u32).min(max_h);
    (width.max(2), height.max(2))
}

impl Capture {
    pub fn start(display_index: usize, limit: Option<(u32, u32)>) -> Result<Self, PlatformError> {
        let content = SCShareableContent::get().map_err(PlatformError::new)?;
        let displays = content.displays();
        let display = displays.get(display_index).ok_or_else(|| {
            PlatformError::new(format!(
                "display {display_index} is not connected ({count} available)",
                count = displays.len()
            ))
        })?;
        let frame = display.frame();
        let native_width = even(display.width());
        let native_height = even(display.height());
        let (pixel_width, pixel_height) = match limit {
            Some((max_w, max_h)) => fit_capture_size(native_width, native_height, max_w, max_h),
            None => (native_width, native_height),
        };
        let refresh = refresh_millihz(display.display_id());
        let fps = (refresh / 1000).clamp(30, 120);
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build()
            .map_err(PlatformError::new)?;
        let latest = Arc::new(Latest {
            frame: Mutex::new(None),
            cv: Condvar::new(),
            width: pixel_width,
            height: pixel_height,
        });
        let audio = Arc::new(AudioSlot {
            pending: Mutex::new(PendingAudio { rate: 0, channels: 0, pcm: Vec::new() }),
            logged: AtomicBool::new(false),
        });
        let stream = match open_capture(&filter, pixel_width, pixel_height, fps, true, &latest, &audio) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("system audio unavailable ({err}); continuing with picture only");
                open_capture(&filter, pixel_width, pixel_height, fps, false, &latest, &audio).map_err(|err| {
                    PlatformError::new(format!(
                        "{err}. Grant Screen Recording to the terminal that launches deskbridge, then run it again."
                    ))
                })?
            }
        };
        Ok(Self {
            stream,
            latest,
            audio,
            info: DisplayInfo {
                width: pixel_width,
                height: pixel_height,
                refresh_millihz: refresh,
            },
            geometry: DisplayGeometry {
                pixel_width,
                pixel_height,
                origin_x: frame.origin.x,
                origin_y: frame.origin.y,
                // Point size stays the display frame, so a smaller picture still maps
                // the pointer across the whole display.
                point_width: frame.size.width,
                point_height: frame.size.height,
            },
            fps,
        })
    }

    /// PCM captured since the last take, interleaved. Empty when the Mac is
    /// between audio callbacks.
    pub fn take_audio(&self) -> Option<PcmChunk> {
        let mut pending = self.audio.pending.lock().expect("audio");
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
        let mut guard = self.latest.frame.lock().expect("capture slot");
        if let Some(frame) = guard.take() {
            return Some(frame);
        }
        let (mut guard, _) = self
            .latest
            .cv
            .wait_timeout(guard, timeout)
            .expect("capture slot");
        guard.take()
    }

    pub fn stop(&self) {
        let _ = self.stream.stop_capture();
    }
}

fn stream_config(width: u32, height: u32, fps: u32, audio: bool) -> SCStreamConfiguration {
    let config = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        // Without this, ScreenCaptureKit keeps the display's full pixel size
        // and the viewer's resolution never reaches the encoder.
        .with_scales_to_fit(true)
        .with_pixel_format(PixelFormat::BGRA)
        .with_shows_cursor(false)
        .with_queue_depth(3)
        .with_minimum_frame_interval(&CMTime::new(1, fps as i32));
    if audio {
        config
            .with_captures_audio(true)
            .with_excludes_current_process_audio(true)
            .with_sample_rate(48_000)
            .with_channel_count(2)
    } else {
        config
    }
}

fn open_capture(
    filter: &SCContentFilter,
    width: u32,
    height: u32,
    fps: u32,
    audio: bool,
    latest: &Arc<Latest>,
    sound: &Arc<AudioSlot>,
) -> Result<SCStream, PlatformError> {
    let config = stream_config(width, height, fps, audio);
    let mut stream = SCStream::new(filter, &config).map_err(PlatformError::new)?;
    let slot = Arc::clone(latest);
    stream
        .add_output_handler(
            move |sample: CMSampleBuffer, _kind: SCStreamOutputType| {
                push_sample(&slot, sample);
            },
            SCStreamOutputType::Screen,
        )
        .map_err(PlatformError::new)?;
    if audio {
        let sound = Arc::clone(sound);
        stream
            .add_output_handler(
                move |sample: CMSampleBuffer, _kind: SCStreamOutputType| {
                    push_audio(&sound, sample);
                },
                SCStreamOutputType::Audio,
            )
            .map_err(PlatformError::new)?;
    }
    stream.start_capture().map_err(PlatformError::new)?;
    Ok(stream)
}

fn push_sample(slot: &Latest, sample: CMSampleBuffer) {
    let status = sample.frame_status();
    if matches!(
        status,
        Some(
            SCFrameStatus::Idle
                | SCFrameStatus::Blank
                | SCFrameStatus::Suspended
                | SCFrameStatus::Stopped
        )
    ) {
        return;
    }
    let Some(buffer) = sample.pixel_buffer() else {
        return;
    };
    let Some(surface) = buffer.io_surface() else {
        return;
    };
    let captured = CapturedFrame {
        surface,
        capture_us: mono_us(),
        changed: changed_fraction(slot, &sample),
    };
    *slot.frame.lock().expect("capture slot") = Some(captured);
    slot.cv.notify_one();
}

/// `None` from ScreenCaptureKit means the attachment was missing or empty.
/// Both come back as no rectangles, so the fraction stays unknown.
fn changed_fraction(slot: &Latest, sample: &CMSampleBuffer) -> Option<f32> {
    let rects = sample.dirty_rects()?;
    let sizes: Vec<(f64, f64)> = rects
        .iter()
        .map(|rect| (rect.size.width, rect.size.height))
        .collect();
    crate::pace::changed_fraction(slot.width, slot.height, Some(&sizes))
}

fn push_audio(slot: &AudioSlot, sample: CMSampleBuffer) {
    let Some((rate, channels, pcm)) = interleaved_pcm(&sample) else {
        if !slot.logged.swap(true, Ordering::Relaxed) {
            eprintln!("system audio arrived in an unexpected format");
        }
        return;
    };
    let mut pending = slot.pending.lock().expect("audio");
    if pending.rate != 0 && (pending.rate != rate || pending.channels != channels) {
        pending.pcm.clear();
    }
    pending.rate = rate;
    pending.channels = channels;
    pending.pcm.extend(pcm);
    let cap = rate as usize * usize::from(channels);
    if pending.pcm.len() > cap {
        let extra = pending.pcm.len() - cap;
        let step = usize::from(channels).max(1);
        pending.pcm.drain(..extra / step * step);
    }
}

fn interleaved_pcm(sample: &CMSampleBuffer) -> Option<(u32, u8, Vec<f32>)> {
    let format = sample.format_description()?;
    let rate = format.audio_sample_rate()? as u32;
    if !(8_000..=96_000).contains(&rate) {
        return None;
    }
    let list = sample.audio_buffer_list().ok()?;
    if list.num_buffers() == 0 {
        return None;
    }
    let is_float = format.audio_is_float();
    let bits = format.audio_bits_per_channel().unwrap_or(if is_float { 32 } else { 16 });
    let be = format.audio_is_big_endian();
    let sample_bytes = if is_float || bits > 16 { 4 } else { 2 };
    if list.num_buffers() == 1 {
        let buf = list.get(0)?;
        let channels = u8::try_from(buf.number_channels).unwrap_or(0);
        let channels = if channels == 0 {
            u8::try_from(format.audio_channel_count()?).ok()?
        } else {
            channels
        };
        if channels == 0 || channels > 2 {
            return None;
        }
        let data = buf.data();
        let frame_bytes = sample_bytes * usize::from(channels);
        if frame_bytes == 0 || data.len() < frame_bytes {
            return None;
        }
        let frames = data.len() / frame_bytes;
        let mut pcm = Vec::with_capacity(frames * usize::from(channels));
        for frame in 0..frames {
            for channel in 0..usize::from(channels) {
                let off = frame * frame_bytes + channel * sample_bytes;
                pcm.push(read_sample(&data[off..off + sample_bytes], is_float, be));
            }
        }
        return Some((rate, channels, pcm));
    }
    let channels = u8::try_from(list.num_buffers().min(2)).ok()?;
    let mut planes = Vec::with_capacity(usize::from(channels));
    for index in 0..usize::from(channels) {
        planes.push(list.get(index)?.data());
    }
    let frames = planes[0].len() / sample_bytes;
    let mut pcm = Vec::with_capacity(frames * usize::from(channels));
    for frame in 0..frames {
        for plane in &planes {
            let off = frame * sample_bytes;
            if off + sample_bytes > plane.len() {
                return None;
            }
            pcm.push(read_sample(&plane[off..off + sample_bytes], is_float, be));
        }
    }
    Some((rate, channels, pcm))
}

fn read_sample(bytes: &[u8], is_float: bool, big_endian: bool) -> f32 {
    if is_float && bytes.len() >= 4 {
        let raw = if big_endian {
            f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        return raw.clamp(-1.0, 1.0);
    }
    if bytes.len() >= 2 {
        let raw = if big_endian {
            i16::from_be_bytes([bytes[0], bytes[1]])
        } else {
            i16::from_le_bytes([bytes[0], bytes[1]])
        };
        return f32::from(raw) / 32768.0;
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::{fit_capture_size, Capture};
    use std::time::Duration;

    #[test]
    fn a_limit_above_native_does_not_scale_up() {
        assert_eq!(fit_capture_size(1800, 1168, 3840, 2160), (1800, 1168));
    }

    #[test]
    fn a_resolution_limit_scales_the_captured_frame() {
        let capture = match Capture::start(0, Some((1280, 720))) {
            Ok(capture) => capture,
            Err(err) => {
                eprintln!("skipping live capture check: {err}");
                return;
            }
        };
        let frame = capture.wait_frame(Duration::from_secs(2));
        capture.stop();
        let frame = frame.expect("a frame");
        let width = frame.surface.width();
        let height = frame.surface.height();
        assert!(
            width <= 1280 && height <= 720,
            "captured {width}x{height}, wanted at most 1280x720"
        );
        assert!(width >= 2 && height >= 2, "{width}x{height}");
    }

    #[test]
    fn a_720p_box_keeps_aspect_and_even_sides() {
        let (width, height) = fit_capture_size(1800, 1168, 1280, 720);
        assert_eq!(width % 2, 0);
        assert_eq!(height % 2, 0);
        assert!(width <= 1280 && height <= 720);
        assert!(width >= 2 && height >= 2);
        let native = 1800.0 / 1168.0;
        let fitted = f64::from(width) / f64::from(height);
        assert!((native - fitted).abs() / native < 0.02, "{width}x{height}");
    }

    #[test]
    fn odd_native_pixels_are_rounded_down_before_the_limit() {
        assert_eq!(fit_capture_size(1801, 1169, 4000, 4000), (1800, 1168));
    }
}
