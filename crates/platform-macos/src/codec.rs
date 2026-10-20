use deskbridge_proto::CodecId;
use videotoolbox::encoder_list::available_video_encoder_details;
use videotoolbox::session::Codec;
use videotoolbox::DecompressionSession;

use crate::error::PlatformError;

pub fn vt_codec(codec: CodecId) -> Codec {
    match codec {
        CodecId::H264 => Codec::H264,
        CodecId::Hevc => Codec::HEVC,
    }
}

/// Hardware encoders on this Mac, HEVC first.
pub fn hardware_encode_codecs() -> Vec<CodecId> {
    let details = available_video_encoder_details().unwrap_or_default();
    let has = |codec: Codec| {
        details.iter().any(|entry| {
            entry.base.codec_type == codec.as_cm_codec_type() && entry.is_hardware_accelerated == Some(true)
        })
    };
    let mut codecs = Vec::new();
    if has(Codec::HEVC) {
        codecs.push(CodecId::Hevc);
    }
    if has(Codec::H264) {
        codecs.push(CodecId::H264);
    }
    codecs
}

/// Hardware decoders on this Mac, HEVC first.
pub fn hardware_decode_codecs() -> Vec<CodecId> {
    let mut codecs = Vec::new();
    if DecompressionSession::is_hardware_decode_supported(Codec::HEVC) {
        codecs.push(CodecId::Hevc);
    }
    if DecompressionSession::is_hardware_decode_supported(Codec::H264) {
        codecs.push(CodecId::H264);
    }
    codecs
}

pub fn require_encode_codec() -> Result<Vec<CodecId>, PlatformError> {
    let codecs = hardware_encode_codecs();
    if codecs.is_empty() {
        return Err(PlatformError::new(
            "this Mac has no hardware H.264 or HEVC encoder",
        ));
    }
    Ok(codecs)
}

pub fn even(value: u32) -> u32 {
    let value = value.max(2);
    value - (value % 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_dimensions_drop_the_odd_pixel() {
        assert_eq!(even(1920), 1920);
        assert_eq!(even(1081), 1080);
        assert_eq!(even(1), 2);
    }

    #[test]
    fn this_mac_advertises_a_hardware_encoder() {
        let codecs = hardware_encode_codecs();
        assert!(
            !codecs.is_empty(),
            "expected a hardware H.264 or HEVC encoder"
        );
    }
}
