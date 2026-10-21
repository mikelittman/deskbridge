//! How long to wait before encoding another frame.
//!
//! A link that is keeping up with the configured bitrate sends at the display
//! rate, including when a large area of the screen changes. A link that has
//! fallen behind waits for the bytes it just sent, and sends fewer frames when
//! a large area is changing so each one can stay sharp.

use std::time::Duration;

/// Waits longer after a large frame so the link can finish it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FramePace {
    bitrate: u32,
    target_bps: u32,
    min_gap: Duration,
    max_gap: Duration,
    not_before: std::time::Instant,
}

impl FramePace {
    pub fn new(bitrate: u32, display_fps: u32) -> Self {
        let fps = display_fps.clamp(1, 60);
        let bitrate = bitrate.max(1);
        Self {
            bitrate,
            target_bps: bitrate,
            min_gap: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            max_gap: Duration::from_millis(200),
            not_before: std::time::Instant::now(),
        }
    }

    pub fn due(&self, now: std::time::Instant) -> bool {
        now >= self.not_before
    }

    /// The rate the link can carry right now. The configured target stays put.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = bitrate.max(1);
    }

    pub fn observe(&mut self, now: std::time::Instant, bytes: usize, changed: Option<f32>) {
        self.not_before = now
            + gap_after_frame(
                self.bitrate,
                self.target_bps,
                self.min_gap,
                self.max_gap,
                bytes,
                changed,
            );
    }
}

/// Encoder frame rate hint.
///
/// A link that can carry the configured bitrate stays at the display rate.
/// A tight link sends fewer pictures when a large area is changing, so each
/// one keeps its detail.
pub fn encode_fps(display_fps: u32, changed: Option<f32>, pace_bps: u32, target_bps: u32) -> u32 {
    let cap = display_fps.clamp(1, 60);
    if pace_bps >= target_bps.max(1) {
        return cap;
    }
    match changed {
        Some(fraction) if fraction >= 0.5 => cap.min(12).max(1),
        Some(fraction) if fraction >= 0.15 => cap.min(24).max(1),
        _ => cap,
    }
}

/// Picture size for a link step. 0 is full, 1 is about two thirds, 2 is half.
pub fn picture_size(width: u32, height: u32, spatial: u8) -> (u32, u32) {
    let (numer, denom) = match spatial {
        0 => (1, 1),
        1 => (2, 3),
        _ => (1, 2),
    };
    let w = (width.max(1) * numer / denom).max(2) & !1;
    let h = (height.max(1) * numer / denom).max(2) & !1;
    (w, h)
}

pub fn gap_after_frame(
    pace_bps: u32,
    target_bps: u32,
    min_gap: Duration,
    max_gap: Duration,
    bytes: usize,
    changed: Option<f32>,
) -> Duration {
    if pace_bps >= target_bps.max(1) {
        return min_gap;
    }
    let display_fps = {
        let us = min_gap.as_micros().max(1);
        (1_000_000u128 / us).clamp(1, 60) as u32
    };
    let fps = encode_fps(display_fps, changed, pace_bps, target_bps).max(1);
    let fps_floor = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let budget = (u64::from(pace_bps.max(1)) / 8).max(1);
    let byte_us = (bytes as u64).saturating_mul(1_000_000) / budget;
    let gap = Duration::from_micros(byte_us).max(fps_floor);
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
        let gap = gap_after_frame(
            20_000_000,
            20_000_000,
            min_gap,
            Duration::from_millis(200),
            4_000,
            Some(0.01),
        );
        assert_eq!(gap, min_gap);
        assert_eq!(encode_fps(120, Some(0.01), 20_000_000, 20_000_000), 60);
    }

    #[test]
    fn a_large_update_on_a_fast_link_stays_at_the_frame_rate() {
        let min_gap = Duration::from_millis(16);
        let gap = gap_after_frame(
            20_000_000,
            20_000_000,
            min_gap,
            Duration::from_millis(200),
            400_000,
            Some(0.8),
        );
        assert_eq!(gap, min_gap);
        assert_eq!(encode_fps(60, Some(0.8), 20_000_000, 20_000_000), 60);
    }

    #[test]
    fn a_large_update_on_a_tight_link_waits() {
        let gap = gap_after_frame(
            1_000_000,
            20_000_000,
            Duration::from_millis(16),
            Duration::from_millis(200),
            400_000,
            Some(0.8),
        );
        assert_eq!(gap, Duration::from_millis(200));
        assert_eq!(encode_fps(60, Some(0.8), 1_000_000, 20_000_000), 12);
        assert_eq!(encode_fps(60, Some(0.2), 12_000_000, 20_000_000), 24);
    }

    #[test]
    fn a_small_update_on_a_tight_link_still_moves() {
        let gap = gap_after_frame(
            1_000_000,
            20_000_000,
            Duration::from_millis(16),
            Duration::from_millis(200),
            4_000,
            Some(0.01),
        );
        assert_eq!(gap, Duration::from_millis(32));
        assert_eq!(encode_fps(60, Some(0.01), 1_000_000, 20_000_000), 60);
    }

    #[test]
    fn an_unknown_change_on_a_tight_link_follows_the_byte_budget() {
        let gap = gap_after_frame(
            2_000_000,
            8_000_000,
            Duration::from_millis(16),
            Duration::from_millis(200),
            1_000_000,
            None,
        );
        assert_eq!(gap, Duration::from_millis(200));
        assert_eq!(encode_fps(60, None, 8_000_000, 8_000_000), 60);
    }

    #[test]
    fn a_tight_link_shrinks_the_picture_in_steps() {
        assert_eq!(picture_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(picture_size(1920, 1080, 1), (1280, 720));
        assert_eq!(picture_size(1920, 1080, 2), (960, 540));
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
