//! macOS capture, hardware encode and decode, Metal present, and input.

mod capture;
mod cg;
mod pace;
mod codec;
mod cursor;
mod decode;
mod encode;
mod error;
mod host;
mod input;
mod permissions;
mod present;
mod preview;

pub use capture::{Capture, DisplayGeometry};
pub use codec::{hardware_decode_codecs, hardware_encode_codecs, require_encode_codec};
pub use decode::Decoder;
pub use error::PlatformError;
pub use host::{Host, HostOptions, Outbound};
pub use permissions::{ensure_host_permissions, PermissionSubject};
pub use present::{content_pixel, letterbox, CursorDraw, Letterbox, Renderer};

#[cfg(test)]
mod tests {
    use super::*;
    use apple_cf::iosurface::IOSurface;
    use deskbridge_proto::CodecId;

    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    #[test]
    fn hardware_roundtrip_keeps_a_red_frame_red() {
        let codecs = hardware_encode_codecs();
        let codec = codecs.first().copied().expect("hardware encoder");
        assert!(hardware_decode_codecs().contains(&codec));
        let width = 320usize;
        let height = 180usize;
        let surface = IOSurface::create(width, height, BGRA, 4).expect("surface");
        {
            let mut guard = surface.lock_read_write().expect("lock");
            let dst = guard.base_address_mut().expect("base");
            let row = guard.bytes_per_row();
            for y in 0..height {
                for x in 0..width {
                    unsafe {
                        let pixel = dst.add(y * row + x * 4);
                        *pixel = 16;
                        *pixel.add(1) = 16;
                        *pixel.add(2) = 220;
                        *pixel.add(3) = 255;
                    }
                }
            }
        }
        let mut encoder = encode::Encoder::open(codec, width as u32, height as u32, 8_000_000, 60)
            .expect("encoder");
        assert!(encoder.set_target_fps(15), "expected frame rate");
        let mut decoder = Decoder::new();
        let mut saved = None;
        let mut picture = None;
        for index in 0..4 {
            let Some(packet) = encoder.encode(&surface, 0, index == 0).expect("encode") else {
                continue;
            };
            if let Some(config) = packet.config {
                saved = Some(config);
            }
            let Some(config) = saved.clone() else { continue };
            if let Some(surface) = decoder.decode(&config, &packet.frame).expect("decode") {
                picture = Some(surface);
            }
        }
        let picture = picture.expect("decoded picture");
        let bgra = if picture.pixel_format() == BGRA {
            picture
        } else {
            let buffer = apple_cf::cv::CVPixelBuffer::create_with_io_surface(&picture)
                .expect("wrap decoded surface");
            decode::copy_to_bgra(&buffer).expect("transfer to BGRA")
        };
        let pixel = decode::sample_bgra(&bgra, width / 2, height / 2).expect("pixel");
        assert!(
            pixel[2] > 140 && pixel[0] < 90 && pixel[1] < 90,
            "center pixel BGRA {pixel:?} is not red ({})",
            codec.name()
        );
        let _ = CodecId::Hevc;
    }
}
