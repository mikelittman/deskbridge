//! How long to wait before encoding another frame.
//!
//! A small update is cheap, so it can go out at the full rate and stay sharp.
//! A large update spends the bitrate on that one picture and the next one waits,
//! which keeps the link from filling up with video.

use std::time::Duration;

/// Waits longer after a large frame so the link can finish it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FramePace {
    bitrate: u32,
    min_gap: Duration,
    max_gap: Duration,
    not_before: std::time::Instant,
}

impl FramePace {
    pub fn new(bitrate: u32, display_fps: u32) -> Self {
        let fps = display_fps.clamp(1, 60);
        Self {
            bitrate: bitrate.max(1),
            min_gap: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            max_gap: Duration::from_millis(200),
            not_before: std::time::Instant::now(),
        }
    }

    pub fn due(&self, now: std::time::Instant) -> bool {
        now >= self.not_before
    }

    pub fn observe(&mut self, now: std::time::Instant, bytes: usize, changed: Option<f32>) {
        self.not_before = now + gap_after_frame(self.bitrate, self.min_gap, self.max_gap, bytes, changed);
    }
}

/// Encoder frame rate hint. A large moving region gets fewer, larger pictures.
pub fn encode_fps(display_fps: u32, changed: Option<f32>) -> u32 {
    let cap = display_fps.clamp(1, 60);
    match changed {
        Some(fraction) if fraction >= 0.5 => cap.min(15).max(1),
        Some(fraction) if fraction >= 0.15 => cap.min(30).max(1),
        _ => cap,
    }
}

pub fn gap_after_frame(
    bitrate: u32,
    min_gap: Duration,
    max_gap: Duration,
    bytes: usize,
    changed: Option<f32>,
) -> Duration {
    let budget = (u64::from(bitrate.max(1)) / 8).max(1);
    let byte_us = (bytes as u64).saturating_mul(1_000_000) / budget;
    let mut gap = Duration::from_micros(byte_us);
    if let Some(fraction) = changed {
        if fraction >= 0.5 {
            gap = gap.max(Duration::from_millis(33));
        } else if fraction >= 0.15 {
            gap = gap.max(Duration::from_millis(16));
        }
    }
    if gap < min_gap {
        min_gap
    } else if gap > max_gap {
        max_gap
    } else {
        gap
    }
}

/// Share of the frame covered by dirty rectangles, from 0 to 1.
/// `None` means the capture did not say. Empty means nothing moved.
pub fn changed_fraction(frame_w: u32, frame_h: u32, rects: Option<&[(f64, f64)]>) -> Option<f32> {
    let rects = rects?;
    if rects.is_empty() {
        return Some(0.0);
    }
    let frame = f64::from(frame_w.max(1)) * f64::from(frame_h.max(1));
    let mut area = 0.0;
    for &(width, height) in rects {
        if width > 0.0 && height > 0.0 {
            area += width * height;
        }
    }
    Some(((area / frame).clamp(0.0, 1.0)) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_update_stays_at_the_full_rate() {
        let min_gap = Duration::from_millis(16);
        let gap = gap_after_frame(20_000_000, min_gap, Duration::from_millis(200), 4_000, Some(0.01));
        assert_eq!(gap, min_gap);
        assert_eq!(encode_fps(120, Some(0.01)), 60);
    }

    #[test]
    fn a_large_update_waits_so_the_picture_can_stay_sharp() {
        let gap = gap_after_frame(
            20_000_000,
            Duration::from_millis(16),
            Duration::from_millis(200),
            400_000,
            Some(0.8),
        );
        assert!(gap >= Duration::from_millis(33), "{gap:?}");
        assert!(gap <= Duration::from_millis(200), "{gap:?}");
        assert_eq!(encode_fps(60, Some(0.8)), 15);
    }

    #[test]
    fn an_unknown_change_follows_the_byte_budget() {
        let gap = gap_after_frame(
            8_000_000,
            Duration::from_millis(16),
            Duration::from_millis(200),
            1_000_000,
            None,
        );
        assert_eq!(gap, Duration::from_millis(200));
        assert_eq!(encode_fps(60, None), 60);
    }

    #[test]
    fn dirty_rects_become_a_fraction_of_the_frame() {
        assert_eq!(changed_fraction(100, 100, None), None);
        assert_eq!(changed_fraction(100, 100, Some(&[])), Some(0.0));
        let fraction = changed_fraction(100, 100, Some(&[(50.0, 50.0)])).unwrap();
        assert!((fraction - 0.25).abs() < 0.001, "{fraction}");
        assert_eq!(changed_fraction(10, 10, Some(&[(100.0, 100.0)])), Some(1.0));
    }
}
