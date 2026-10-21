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

pub struct Capture {
    stream: SCStream,
    latest: Arc<Latest>,
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
        let config = SCStreamConfiguration::new()
            .with_width(pixel_width)
            .with_height(pixel_height)
            // Without this, ScreenCaptureKit keeps the display's full pixel size
            // and the viewer's resolution never reaches the encoder.
            .with_scales_to_fit(true)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(false)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&CMTime::new(1, fps as i32));
        let mut stream = SCStream::new(&filter, &config).map_err(PlatformError::new)?;
        let latest = Arc::new(Latest {
            frame: Mutex::new(None),
            cv: Condvar::new(),
            width: pixel_width,
            height: pixel_height,
        });
        let slot = Arc::clone(&latest);
        stream
            .add_output_handler(
                move |sample: CMSampleBuffer, _kind: SCStreamOutputType| {
                    push_sample(&slot, sample);
                },
                SCStreamOutputType::Screen,
            )
            .map_err(PlatformError::new)?;
        stream.start_capture().map_err(|err| {
            PlatformError::new(format!(
                "{err}. Grant Screen Recording to the terminal that launches deskbridge, then run it again."
            ))
        })?;
        Ok(Self {
            stream,
            latest,
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
