//! VA-API VLD decode. Frames are AVCC; parameter sets arrive without start codes.

use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};

use crate::codec::{
    copy_bgra_rows, h264_nal_type, hevc_nal_type, image_format, nv12_to_bgra, split_avcc, PictureH264,
    PictureHevc, VaApi, BUF_IQ_MATRIX, BUF_PICTURE, BUF_SLICE_DATA, BUF_SLICE_PARAM, CONFIG_RT_FORMAT,
    PICTURE_H264_INVALID, PICTURE_H264_SHORT_TERM, VA_FOURCC_BGRA, VA_FOURCC_NV12, VA_INVALID_ID,
    VA_PROGRESSIVE, VA_RT_FORMAT_YUV420,
};
use crate::error::PlatformError;

const PROFILE_H264_MAIN: i32 = 6;
const PROFILE_H264_HIGH: i32 = 7;
const PROFILE_HEVC_MAIN: i32 = 17;
const ENTRY_VLD: i32 = 1;
const SURFACES: usize = 8;

pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub struct Decoder {
    api: Option<VaApi>,
    codec: Option<CodecId>,
    parameter_sets: Vec<Vec<u8>>,
    config_id: u32,
    context: u32,
    surfaces: Vec<u32>,
    coded_w: u32,
    coded_h: u32,
    visible_w: u32,
    visible_h: u32,
    next_surface: usize,
    h264: Option<H264State>,
    hevc: Option<HevcState>,
    dpb_h264: Vec<RefH264>,
    dpb_hevc: Vec<RefHevc>,
}

struct RefH264 {
    surface: u32,
    frame_num: u32,
    poc: i32,
}

struct RefHevc {
    surface: u32,
    poc: i32,
}

struct H264State {
    sps: SpsH264,
    pps: PpsH264,
}

struct HevcState {
    sps: SpsHevc,
    pps: PpsHevc,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            api: None,
            codec: None,
            parameter_sets: Vec::new(),
            config_id: VA_INVALID_ID,
            context: VA_INVALID_ID,
            surfaces: Vec::new(),
            coded_w: 0,
            coded_h: 0,
            visible_w: 0,
            visible_h: 0,
            next_surface: 0,
            h264: None,
            hevc: None,
            dpb_h264: Vec::new(),
            dpb_hevc: Vec::new(),
        }
    }

    pub fn decode(&mut self, config: &CodecConfig, frame: &VideoFrame) -> Result<Option<Picture>, PlatformError> {
        if frame.payload.is_empty() {
            return Ok(None);
        }
        if !crate::codec::hardware_decode_codecs().contains(&config.codec) && self.api.is_none() {
            return Err(PlatformError::new(format!(
                "no hardware {} decoder",
                config.codec.name()
            )));
        }
        let nals = split_avcc(&frame.payload, config.nal_length_size.max(1))?;
        let mut sets = config.parameter_sets.clone();
        for nal in &nals {
            let kind = match config.codec {
                CodecId::H264 => h264_nal_type(nal),
                CodecId::Hevc => hevc_nal_type(nal),
            };
            let parameter = match config.codec {
                CodecId::H264 => matches!(kind, 7 | 8),
                CodecId::Hevc => matches!(kind, 32 | 33 | 34),
            };
            if parameter {
                sets.push(nal.clone());
            }
        }
        if self.codec != Some(config.codec) || self.parameter_sets != sets || self.api.is_none() {
            self.reset();
            self.open(config.codec, &sets)?;
            self.parameter_sets = sets;
            self.codec = Some(config.codec);
        }
        let slices: Vec<Vec<u8>> = nals
            .into_iter()
            .filter(|nal| match config.codec {
                CodecId::H264 => matches!(h264_nal_type(nal), 1 | 5),
                CodecId::Hevc => hevc_nal_type(nal) <= 31 && !nal.is_empty(),
            })
            .collect();
        if slices.is_empty() {
            return Ok(None);
        }
        match config.codec {
            CodecId::H264 => self.decode_h264(&slices),
            CodecId::Hevc => self.decode_hevc(&slices),
        }
    }

    fn reset(&mut self) {
        if let Some(api) = &self.api {
            let display = api.display();
            if self.context != VA_INVALID_ID {
                unsafe { (api.destroy_context)(display, self.context) };
            }
            if !self.surfaces.is_empty() {
                unsafe { (api.destroy_surfaces)(display, self.surfaces.as_mut_ptr(), self.surfaces.len() as i32) };
            }
            if self.config_id != VA_INVALID_ID {
                unsafe { (api.destroy_config)(display, self.config_id) };
            }
        }
        self.api = None;
        self.context = VA_INVALID_ID;
        self.config_id = VA_INVALID_ID;
        self.surfaces.clear();
        self.h264 = None;
        self.hevc = None;
        self.dpb_h264.clear();
        self.dpb_hevc.clear();
        self.next_surface = 0;
    }

    fn open(&mut self, codec: CodecId, sets: &[Vec<u8>]) -> Result<(), PlatformError> {
        let api = VaApi::open()?;
        let (coded_w, coded_h, visible_w, visible_h, profile) = match codec {
            CodecId::H264 => {
                let sps = sets
                    .iter()
                    .find(|nal| h264_nal_type(nal) == 7)
                    .ok_or_else(|| PlatformError::new("H.264 stream has no SPS"))?;
                let pps = sets
                    .iter()
                    .find(|nal| h264_nal_type(nal) == 8)
                    .ok_or_else(|| PlatformError::new("H.264 stream has no PPS"))?;
                let sps = parse_sps_h264(&rbsp(&sps[1..]))?;
                let pps = parse_pps_h264(&rbsp(&pps[1..]), sps.high)?;
                let profile = if sps.high { PROFILE_H264_HIGH } else { PROFILE_H264_MAIN };
                self.h264 = Some(H264State { sps, pps });
                (self.h264.as_ref().unwrap().sps.coded_w, self.h264.as_ref().unwrap().sps.coded_h, self.h264.as_ref().unwrap().sps.visible_w, self.h264.as_ref().unwrap().sps.visible_h, profile)
            }
            CodecId::Hevc => {
                let sps_nal = sets
                    .iter()
                    .find(|nal| hevc_nal_type(nal) == 33)
                    .ok_or_else(|| PlatformError::new("HEVC stream has no SPS"))?;
                let pps_nal = sets
                    .iter()
                    .find(|nal| hevc_nal_type(nal) == 34)
                    .ok_or_else(|| PlatformError::new("HEVC stream has no PPS"))?;
                let sps = parse_sps_hevc(&rbsp(&sps_nal[2..]))?;
                let pps = parse_pps_hevc(&rbsp(&pps_nal[2..]))?;
                self.hevc = Some(HevcState { sps, pps });
                let state = self.hevc.as_ref().unwrap();
                (state.sps.coded_w, state.sps.coded_h, state.sps.visible_w, state.sps.visible_h, PROFILE_HEVC_MAIN)
            }
        };
        if !api.supports(profile, ENTRY_VLD) {
            return Err(PlatformError::new(format!("no hardware {codec:?} decoder")));
        }
        let mut attrib = crate::codec::ConfigAttrib { kind: CONFIG_RT_FORMAT, value: VA_RT_FORMAT_YUV420 };
        let mut config_id = 0u32;
        api.check(
            unsafe { (api.create_config)(api.display(), profile, ENTRY_VLD, &mut attrib, 1, &mut config_id) },
            "vaCreateConfig",
        )?;
        let mut surfaces = vec![0u32; SURFACES];
        let created = api.check(
            unsafe {
                (api.create_surfaces)(
                    api.display(),
                    VA_RT_FORMAT_YUV420,
                    coded_w,
                    coded_h,
                    surfaces.as_mut_ptr(),
                    SURFACES as u32,
                    std::ptr::null_mut(),
                    0,
                )
            },
            "vaCreateSurfaces",
        );
        if let Err(err) = created {
            unsafe { (api.destroy_config)(api.display(), config_id) };
            return Err(err);
        }
        let mut context = 0u32;
        let opened = api.check(
            unsafe {
                (api.create_context)(
                    api.display(),
                    config_id,
                    coded_w as i32,
                    coded_h as i32,
                    VA_PROGRESSIVE,
                    surfaces.as_mut_ptr(),
                    SURFACES as i32,
                    &mut context,
                )
            },
            "vaCreateContext",
        );
        if let Err(err) = opened {
            unsafe { (api.destroy_surfaces)(api.display(), surfaces.as_mut_ptr(), SURFACES as i32) };
            unsafe { (api.destroy_config)(api.display(), config_id) };
            return Err(err);
        }
        self.api = Some(api);
        self.config_id = config_id;
        self.context = context;
        self.surfaces = surfaces;
        self.coded_w = coded_w;
        self.coded_h = coded_h;
        self.visible_w = visible_w.max(2);
        self.visible_h = visible_h.max(2);
        Ok(())
    }

    fn decode_h264(&mut self, slices: &[Vec<u8>]) -> Result<Option<Picture>, PlatformError> {
        let state = self.h264.as_ref().ok_or_else(|| PlatformError::new("H.264 decoder is not open"))?;
        let sps = state.sps;
        let pps = state.pps;
        let surface = self.surfaces[self.next_surface % self.surfaces.len()];
        let mut headers = Vec::new();
        for slice in slices {
            headers.push(parse_slice_h264(&rbsp(slice), &sps, &pps)?);
        }
        let header = headers[0];
        let idr = slices.iter().any(|nal| h264_nal_type(nal) == 5);
        if idr {
            self.dpb_h264.clear();
        }
        let api = self.api.as_ref().unwrap();
        let display = api.display();
        let mut bufs = Vec::new();
        let pic = picture_h264(surface, &sps, &pps, &header, &self.dpb_h264);
        bufs.push(make_buf(api, self.context, BUF_PICTURE, &pic)?);
        for (slice, header) in slices.iter().zip(headers.iter()) {
            let param = slice_param_h264(slice, header, &sps, &self.dpb_h264);
            bufs.push(make_buf(api, self.context, BUF_SLICE_PARAM, &param)?);
            bufs.push(make_slice_data(api, self.context, slice)?);
        }
        let rendered = render(api, self.context, surface, &mut bufs);
        for id in &bufs {
            unsafe { (api.destroy_buffer)(display, *id) };
        }
        rendered?;
        let picture = read_picture(api, surface, self.visible_w, self.visible_h)?;
        let ref_idc = slices[0].first().copied().unwrap_or(0) >> 5;
        if ref_idc != 0 {
            self.dpb_h264.push(RefH264 {
                surface,
                frame_num: u32::from(header.frame_num),
                poc: header.poc,
            });
            if self.dpb_h264.len() > 6 {
                self.dpb_h264.remove(0);
            }
            self.next_surface = (self.next_surface + 1) % self.surfaces.len();
        }
        Ok(Some(picture))
    }

    fn decode_hevc(&mut self, slices: &[Vec<u8>]) -> Result<Option<Picture>, PlatformError> {
        let state = self.hevc.as_ref().ok_or_else(|| PlatformError::new("HEVC decoder is not open"))?;
        let sps = state.sps;
        let pps = state.pps;
        let surface = self.surfaces[self.next_surface % self.surfaces.len()];
        let mut headers = Vec::new();
        for slice in slices {
            headers.push(parse_slice_hevc(slice, &sps, &pps)?);
        }
        let header = &headers[0];
        if header.idr {
            self.dpb_hevc.clear();
        }
        let api = self.api.as_ref().unwrap();
        let display = api.display();
        let mut bufs = Vec::new();
        let pic = picture_hevc(surface, &sps, &pps, header, &self.dpb_hevc);
        bufs.push(make_buf(api, self.context, BUF_PICTURE, &pic)?);
        let _ = BUF_IQ_MATRIX;
        for (index, slice) in slices.iter().enumerate() {
            let param = slice_param_hevc(slice, &headers[index], &self.dpb_hevc, index + 1 == slices.len());
            bufs.push(make_buf(api, self.context, BUF_SLICE_PARAM, &param)?);
            bufs.push(make_slice_data(api, self.context, slice)?);
        }
        let rendered = render(api, self.context, surface, &mut bufs);
        for id in &bufs {
            unsafe { (api.destroy_buffer)(display, *id) };
        }
        rendered?;
        let picture = read_picture(api, surface, self.visible_w, self.visible_h)?;
        self.dpb_hevc.push(RefHevc { surface, poc: header.poc });
        if self.dpb_hevc.len() > 6 {
            self.dpb_hevc.remove(0);
        }
        self.next_surface = (self.next_surface + 1) % self.surfaces.len();
        Ok(Some(picture))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.reset();
    }
}

fn render(api: &VaApi, context: u32, surface: u32, bufs: &mut [u32]) -> Result<(), PlatformError> {
    let display = api.display();
    api.check(unsafe { (api.begin_picture)(display, context, surface) }, "vaBeginPicture")?;
    let status = unsafe { (api.render_picture)(display, context, bufs.as_mut_ptr(), bufs.len() as i32) };
    let rendered = api.check(status, "vaRenderPicture");
    let ended = api.check(unsafe { (api.end_picture)(display, context) }, "vaEndPicture");
    rendered?;
    ended?;
    api.check(unsafe { (api.sync_surface)(display, surface) }, "vaSyncSurface")
}

fn make_buf<T>(api: &VaApi, context: u32, kind: i32, value: &T) -> Result<u32, PlatformError> {
    let mut id = 0u32;
    api.check(
        unsafe {
            (api.create_buffer)(
                api.display(),
                context,
                kind,
                std::mem::size_of::<T>() as u32,
                1,
                (value as *const T).cast_mut().cast(),
                &mut id,
            )
        },
        "vaCreateBuffer",
    )?;
    Ok(id)
}

fn make_slice_data(api: &VaApi, context: u32, nal: &[u8]) -> Result<u32, PlatformError> {
    let mut id = 0u32;
    api.check(
        unsafe {
            (api.create_buffer)(
                api.display(),
                context,
                BUF_SLICE_DATA,
                nal.len() as u32,
                1,
                nal.as_ptr().cast_mut().cast(),
                &mut id,
            )
        },
        "vaCreateBuffer",
    )?;
    Ok(id)
}

fn read_picture(api: &VaApi, surface: u32, width: u32, height: u32) -> Result<Picture, PlatformError> {
    let display = api.display();
    let mut bgra_fmt = image_format(VA_FOURCC_BGRA, 32);
    let mut image = unsafe { std::mem::zeroed::<crate::codec::Image>() };
    let bgra_ok = api.check(
        unsafe { (api.create_image)(display, &mut bgra_fmt, width as i32, height as i32, &mut image) },
        "vaCreateImage",
    );
    let use_bgra = bgra_ok.is_ok();
    if !use_bgra {
        let mut nv12 = image_format(VA_FOURCC_NV12, 12);
        api.check(
            unsafe { (api.create_image)(display, &mut nv12, width as i32, height as i32, &mut image) },
            "vaCreateImage",
        )?;
    }
    let got = api.check(
        unsafe { (api.get_image)(display, surface, 0, 0, width, height, image.image_id) },
        "vaGetImage",
    );
    if let Err(err) = got {
        unsafe { (api.destroy_image)(display, image.image_id) };
        return Err(err);
    }
    let mut mapped = std::ptr::null_mut();
    let mapped_ok = api.check(unsafe { (api.map_buffer)(display, image.buf, &mut mapped) }, "vaMapBuffer");
    if let Err(err) = mapped_ok {
        unsafe { (api.destroy_image)(display, image.image_id) };
        return Err(err);
    }
    let size = image.data_size.max(1) as usize;
    let bgra = unsafe {
        let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), size);
        if use_bgra {
            copy_bgra_rows(bytes, width, height, image.pitches[0].max(width * 4) as usize)
        } else {
            nv12_to_bgra(bytes, width, height, image.pitches, image.offsets)
        }
    };
    let _ = api.check(unsafe { (api.unmap_buffer)(display, image.buf) }, "vaUnmapBuffer");
    unsafe { (api.destroy_image)(display, image.image_id) };
    Ok(Picture { width, height, bgra })
}

fn rbsp(nal_after_header: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_after_header.len());
    let mut zeros = 0;
    for byte in nal_after_header {
        if zeros >= 2 && *byte == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(*byte);
        if *byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }
    fn bit(&mut self) -> Result<u32, PlatformError> {
        let index = self.bit / 8;
        if index >= self.data.len() {
            return Err(PlatformError::new("truncated parameter set"));
        }
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        Ok(u32::from((self.data[index] >> shift) & 1))
    }
    fn bits(&mut self, n: u32) -> Result<u32, PlatformError> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32, PlatformError> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(PlatformError::new("exp-golomb is too wide"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)?;
        Ok(((1u32 << zeros) - 1) + rest)
    }
    fn se(&mut self) -> Result<i32, PlatformError> {
        let value = self.ue()?;
        let magnitude = ((value + 1) >> 1) as i32;
        if value & 1 == 0 { Ok(-magnitude) } else { Ok(magnitude) }
    }
    fn used(&self) -> usize {
        self.bit
    }
}

#[derive(Clone, Copy)]
struct SpsH264 {
    high: bool,
    chroma_format_idc: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
    log2_max_frame_num_minus4: u32,
    pic_order_cnt_type: u32,
    log2_max_pic_order_cnt_lsb_minus4: u32,
    delta_pic_order_always_zero: bool,
    max_num_ref_frames: u32,
    gaps_in_frame_num_value_allowed: bool,
    frame_mbs_only: bool,
    mb_adaptive_frame_field: bool,
    direct_8x8_inference: bool,
    coded_w: u32,
    coded_h: u32,
    visible_w: u32,
    visible_h: u32,
}

#[derive(Clone, Copy)]
struct PpsH264 {
    entropy_coding_mode: bool,
    bottom_field_pic_order_in_frame_present: bool,
    num_ref_idx_l0_default_active_minus1: u32,
    num_ref_idx_l1_default_active_minus1: u32,
    weighted_pred: bool,
    weighted_bipred_idc: u32,
    pic_init_qp_minus26: i32,
    pic_init_qs_minus26: i32,
    chroma_qp_index_offset: i32,
    second_chroma_qp_index_offset: i32,
    deblocking_filter_control_present: bool,
    constrained_intra_pred: bool,
    redundant_pic_cnt_present: bool,
    transform_8x8_mode: bool,
}

#[derive(Clone, Copy)]
struct SliceH264 {
    nal_ref_idc: u32,
    idr: bool,
    first_mb: u32,
    slice_type: u32,
    frame_num: u16,
    idr_pic_id: u32,
    poc: i32,
    pic_order_cnt_lsb: u16,
    num_ref_idx_l0_active_minus1: u32,
    num_ref_idx_l1_active_minus1: u32,
    cabac_init_idc: u32,
    slice_qp_delta: i32,
    disable_deblocking_filter_idc: u32,
    slice_alpha: i32,
    slice_beta: i32,
    header_bits: u32,
}

fn parse_sps_h264(data: &[u8]) -> Result<SpsH264, PlatformError> {
    let mut bits = Bits::new(data);
    let profile = bits.bits(8)?;
    let _constraints = bits.bits(8)?;
    let _level = bits.bits(8)?;
    let _sps_id = bits.ue()?;
    let high = matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135);
    let mut chroma = 1u32;
    let mut bit_luma = 0u32;
    let mut bit_chroma = 0u32;
    if high {
        chroma = bits.ue()?;
        if chroma == 3 {
            let _separate = bits.bit()?;
        }
        bit_luma = bits.ue()?;
        bit_chroma = bits.ue()?;
        let _qpprime = bits.bit()?;
        if bits.bit()? == 1 {
            return Err(PlatformError::new("SPS scaling lists are not used"));
        }
    }
    let log2_max_frame_num_minus4 = bits.ue()?;
    let pic_order_cnt_type = bits.ue()?;
    let mut log2_poc = 0u32;
    let mut delta_zero = false;
    if pic_order_cnt_type == 0 {
        log2_poc = bits.ue()?;
    } else if pic_order_cnt_type == 1 {
        delta_zero = bits.bit()? == 1;
        let _off_non_ref = bits.se()?;
        let _off_top = bits.se()?;
        let cycle = bits.ue()?;
        for _ in 0..cycle {
            let _ = bits.se()?;
        }
    }
    let max_num_ref_frames = bits.ue()?;
    let gaps = bits.bit()? == 1;
    let width_mbs = bits.ue()?.saturating_add(1);
    let height_map = bits.ue()?.saturating_add(1);
    let frame_mbs_only = bits.bit()? == 1;
    let mut mbaff = false;
    if !frame_mbs_only {
        mbaff = bits.bit()? == 1;
    }
    let direct_8x8 = bits.bit()? == 1;
    let mut crop_left = 0u32;
    let mut crop_right = 0u32;
    let mut crop_top = 0u32;
    let mut crop_bottom = 0u32;
    if bits.bit()? == 1 {
        crop_left = bits.ue()?;
        crop_right = bits.ue()?;
        crop_top = bits.ue()?;
        crop_bottom = bits.ue()?;
    }
    let coded_w = width_mbs.saturating_mul(16);
    let coded_h = height_map.saturating_mul(if frame_mbs_only { 16 } else { 32 });
    let crop_unit_x = if chroma == 0 { 1 } else { 2 };
    let crop_unit_y = if chroma == 0 { 2 - u32::from(frame_mbs_only) } else { (2 - u32::from(frame_mbs_only)) * 2 };
    let visible_w = coded_w.saturating_sub((crop_left + crop_right) * crop_unit_x);
    let visible_h = coded_h.saturating_sub((crop_top + crop_bottom) * crop_unit_y);
    Ok(SpsH264 {
        high,
        chroma_format_idc: chroma,
        bit_depth_luma_minus8: bit_luma,
        bit_depth_chroma_minus8: bit_chroma,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4: log2_poc,
        delta_pic_order_always_zero: delta_zero,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed: gaps,
        frame_mbs_only,
        mb_adaptive_frame_field: mbaff,
        direct_8x8_inference: direct_8x8,
        coded_w,
        coded_h,
        visible_w: visible_w.max(2),
        visible_h: visible_h.max(2),
    })
}

fn parse_pps_h264(data: &[u8], high: bool) -> Result<PpsH264, PlatformError> {
    let mut bits = Bits::new(data);
    let _pps_id = bits.ue()?;
    let _sps_id = bits.ue()?;
    let entropy = bits.bit()? == 1;
    let bottom = bits.bit()? == 1;
    let groups = bits.ue()?;
    if groups > 0 {
        return Err(PlatformError::new("FMO slices are not supported"));
    }
    let l0 = bits.ue()?;
    let l1 = bits.ue()?;
    let weighted_pred = bits.bit()? == 1;
    let weighted_bipred = bits.bits(2)?;
    let pic_init_qp = bits.se()?;
    let pic_init_qs = bits.se()?;
    let chroma = bits.se()?;
    let deblock = bits.bit()? == 1;
    let constrained = bits.bit()? == 1;
    let redundant = bits.bit()? == 1;
    let mut transform_8x8 = false;
    let mut second = chroma;
    if high && bits.data.len() * 8 > bits.used() + 2 {
        transform_8x8 = bits.bit()? == 1;
        if bits.bit()? == 1 {
            return Err(PlatformError::new("PPS scaling lists are not used"));
        }
        second = bits.se()?;
    }
    Ok(PpsH264 {
        entropy_coding_mode: entropy,
        bottom_field_pic_order_in_frame_present: bottom,
        num_ref_idx_l0_default_active_minus1: l0,
        num_ref_idx_l1_default_active_minus1: l1,
        weighted_pred,
        weighted_bipred_idc: weighted_bipred,
        pic_init_qp_minus26: pic_init_qp,
        pic_init_qs_minus26: pic_init_qs,
        chroma_qp_index_offset: chroma,
        second_chroma_qp_index_offset: second,
        deblocking_filter_control_present: deblock,
        constrained_intra_pred: constrained,
        redundant_pic_cnt_present: redundant,
        transform_8x8_mode: transform_8x8,
    })
}

fn parse_slice_h264(nal: &[u8], sps: &SpsH264, pps: &PpsH264) -> Result<SliceH264, PlatformError> {
    if nal.is_empty() {
        return Err(PlatformError::new("empty slice"));
    }
    let nal_ref_idc = u32::from(nal[0] >> 5);
    let nal_type = nal[0] & 0x1f;
    let idr = nal_type == 5;
    let body = rbsp(&nal[1..]);
    let mut bits = Bits::new(&body);
    let first_mb = bits.ue()?;
    let slice_type = bits.ue()? % 5;
    let _pps_id = bits.ue()?;
    let frame_num = bits.bits(sps.log2_max_frame_num_minus4.saturating_add(4))? as u16;
    if !sps.frame_mbs_only {
        if bits.bit()? == 1 {
            let _bottom = bits.bit()?;
        }
    }
    let mut idr_pic_id = 0u32;
    if idr {
        idr_pic_id = bits.ue()?;
    }
    let mut poc_lsb = 0u16;
    if sps.pic_order_cnt_type == 0 {
        poc_lsb = bits.bits(sps.log2_max_pic_order_cnt_lsb_minus4.saturating_add(4))? as u16;
        if pps.bottom_field_pic_order_in_frame_present {
            let _ = bits.se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        let _ = bits.se()?;
    }
    if pps.redundant_pic_cnt_present {
        let _ = bits.ue()?;
    }
    let mut l0 = pps.num_ref_idx_l0_default_active_minus1;
    let mut l1 = pps.num_ref_idx_l1_default_active_minus1;
    if slice_type == 0 || slice_type == 1 {
        if bits.bit()? == 1 {
            l0 = bits.ue()?;
            if slice_type == 1 {
                l1 = bits.ue()?;
            }
        }
    }
    if nal_ref_idc != 0 {
        if idr {
            let _no_output = bits.bit()?;
            let _long_term = bits.bit()?;
        } else if bits.bit()? == 1 {
            while bits.ue()? != 0 {
                let _ = bits.ue()?;
            }
        }
    }
    let mut cabac_init = 0u32;
    if pps.entropy_coding_mode && slice_type != 2 && slice_type != 4 {
        cabac_init = bits.ue()?;
    }
    let slice_qp_delta = bits.se()?;
    let mut disable = 0u32;
    let mut alpha = 0i32;
    let mut beta = 0i32;
    if pps.deblocking_filter_control_present {
        disable = bits.ue()?;
        if disable != 1 {
            alpha = bits.se()?;
            beta = bits.se()?;
        }
    }
    // Header bit count includes the NAL header byte. The body was parsed on the RBSP.
    let header_bits = 8 + bits.used() as u32;
    Ok(SliceH264 {
        nal_ref_idc,
        idr,
        first_mb,
        slice_type,
        frame_num,
        idr_pic_id,
        poc: i32::from(poc_lsb),
        pic_order_cnt_lsb: poc_lsb,
        num_ref_idx_l0_active_minus1: l0,
        num_ref_idx_l1_active_minus1: l1,
        cabac_init_idc: cabac_init,
        slice_qp_delta,
        disable_deblocking_filter_idc: disable,
        slice_alpha: alpha,
        slice_beta: beta,
        header_bits,
    })
}

fn picture_h264(surface: u32, sps: &SpsH264, pps: &PpsH264, header: &SliceH264, dpb: &[RefH264]) -> DecPictureH264 {
    let mut pic = unsafe { std::mem::zeroed::<DecPictureH264>() };
    pic.curr_pic = PictureH264 {
        picture_id: surface,
        frame_idx: u32::from(header.frame_num),
        flags: if header.nal_ref_idc != 0 { PICTURE_H264_SHORT_TERM } else { 0 },
        top_field_order_cnt: header.poc,
        bottom_field_order_cnt: header.poc,
        va_reserved: [0; 4],
    };
    for slot in &mut pic.reference_frames {
        *slot = PictureH264 {
            picture_id: VA_INVALID_ID,
            frame_idx: 0,
            flags: PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        };
    }
    for (slot, reference) in pic.reference_frames.iter_mut().zip(dpb.iter().rev()) {
        *slot = PictureH264 {
            picture_id: reference.surface,
            frame_idx: reference.frame_num,
            flags: PICTURE_H264_SHORT_TERM,
            top_field_order_cnt: reference.poc,
            bottom_field_order_cnt: reference.poc,
            va_reserved: [0; 4],
        };
    }
    pic.picture_width_in_mbs_minus1 = (sps.coded_w / 16).saturating_sub(1) as u16;
    pic.picture_height_in_mbs_minus1 = (sps.coded_h / 16).saturating_sub(1) as u16;
    pic.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
    pic.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
    pic.num_ref_frames = sps.max_num_ref_frames.min(16) as u8;
    let mut seq = sps.chroma_format_idc & 3;
    seq |= u32::from(sps.frame_mbs_only) << 4;
    seq |= u32::from(sps.mb_adaptive_frame_field) << 5;
    seq |= u32::from(sps.direct_8x8_inference) << 6;
    seq |= (sps.log2_max_frame_num_minus4 & 0xf) << 8;
    seq |= (sps.pic_order_cnt_type & 3) << 12;
    seq |= (sps.log2_max_pic_order_cnt_lsb_minus4 & 0xf) << 14;
    seq |= u32::from(sps.delta_pic_order_always_zero) << 18;
    seq |= u32::from(sps.gaps_in_frame_num_value_allowed) << 3;
    pic.seq_fields = seq;
    pic.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
    pic.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
    pic.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
    pic.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
    let mut fields = u32::from(pps.entropy_coding_mode);
    fields |= u32::from(pps.weighted_pred) << 1;
    fields |= (pps.weighted_bipred_idc & 3) << 2;
    fields |= u32::from(pps.transform_8x8_mode) << 4;
    fields |= u32::from(pps.constrained_intra_pred) << 6;
    fields |= u32::from(pps.bottom_field_pic_order_in_frame_present) << 7;
    fields |= u32::from(pps.deblocking_filter_control_present) << 8;
    fields |= u32::from(pps.redundant_pic_cnt_present) << 9;
    fields |= u32::from(header.nal_ref_idc != 0) << 10;
    pic.pic_fields = fields;
    pic.frame_num = header.frame_num;
    pic
}

fn slice_param_h264(nal: &[u8], header: &SliceH264, sps: &SpsH264, dpb: &[RefH264]) -> DecSliceH264 {
    let mut slice = unsafe { std::mem::zeroed::<DecSliceH264>() };
    slice.slice_data_size = nal.len() as u32;
    slice.slice_data_bit_offset = header.header_bits as u16;
    slice.first_mb_in_slice = header.first_mb as u16;
    slice.slice_type = header.slice_type as u8;
    slice.num_ref_idx_l0_active_minus1 = header.num_ref_idx_l0_active_minus1 as u8;
    slice.num_ref_idx_l1_active_minus1 = header.num_ref_idx_l1_active_minus1 as u8;
    slice.cabac_init_idc = header.cabac_init_idc as u8;
    slice.slice_qp_delta = header.slice_qp_delta as i8;
    slice.disable_deblocking_filter_idc = header.disable_deblocking_filter_idc as u8;
    slice.slice_alpha_c0_offset_div2 = header.slice_alpha as i8;
    slice.slice_beta_offset_div2 = header.slice_beta as i8;
    let invalid = PictureH264 {
        picture_id: VA_INVALID_ID,
        frame_idx: 0,
        flags: PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };
    slice.ref_pic_list0 = [invalid; 32];
    slice.ref_pic_list1 = [invalid; 32];
    if header.slice_type == 0 || header.slice_type == 1 {
        if let Some(reference) = dpb.last() {
            slice.ref_pic_list0[0] = PictureH264 {
                picture_id: reference.surface,
                frame_idx: reference.frame_num,
                flags: PICTURE_H264_SHORT_TERM,
                top_field_order_cnt: reference.poc,
                bottom_field_order_cnt: reference.poc,
                va_reserved: [0; 4],
            };
        }
    }
    let _ = sps;
    slice
}

#[derive(Clone, Copy)]
struct SpsHevc {
    chroma_format_idc: u32,
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    visible_w: u32,
    visible_h: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
    log2_max_pic_order_cnt_lsb_minus4: u32,
    log2_min_luma_coding_block_size_minus3: u32,
    log2_diff_max_min_luma_coding_block_size: u32,
    log2_min_transform_block_size_minus2: u32,
    log2_diff_max_min_transform_block_size: u32,
    max_transform_hierarchy_depth_inter: u32,
    max_transform_hierarchy_depth_intra: u32,
    sample_adaptive_offset: bool,
    temporal_mvp: bool,
    strong_intra: bool,
    amp: bool,
    pcm: bool,
}

#[derive(Clone, Copy)]
struct PpsHevc {
    dependent_slice_segments_enabled: bool,
    output_flag_present: bool,
    num_extra_slice_header_bits: u32,
    cabac_init_present: bool,
    num_ref_idx_l0_default_active_minus1: u32,
    num_ref_idx_l1_default_active_minus1: u32,
    init_qp_minus26: i32,
    constrained_intra: bool,
    transform_skip: bool,
    cu_qp_delta_enabled: bool,
    diff_cu_qp_delta_depth: u32,
    pps_cb_qp_offset: i32,
    pps_cr_qp_offset: i32,
    weighted_pred: bool,
    weighted_bipred: bool,
    transquant_bypass: bool,
    tiles_enabled: bool,
    entropy_coding_sync: bool,
    loop_filter_across_slices: bool,
    deblocking_filter_override_enabled: bool,
    pps_disable_deblocking: bool,
    pps_beta_offset_div2: i32,
    pps_tc_offset_div2: i32,
    lists_modification_present: bool,
    slice_chroma_qp_offsets_present: bool,
    log2_parallel_merge_level_minus2: u32,
}

#[derive(Clone, Copy)]
struct SliceHevc {
    idr: bool,
    slice_type: u32,
    poc: i32,
    slice_qp_delta: i32,
    sao_luma: bool,
    sao_chroma: bool,
    temporal_mvp: bool,
    deblocking_disabled: bool,
    header_bytes: u32,
    num_ref_idx_l0: u32,
    num_ref_idx_l1: u32,
}

fn skip_profile_tier(bits: &mut Bits<'_>) -> Result<(), PlatformError> {
    let _profile_space = bits.bits(2)?;
    let _tier = bits.bit()?;
    let _profile_idc = bits.bits(5)?;
    let _compat = bits.bits(32)?;
    let _flags = bits.bits(32)?;
    let _flags2 = bits.bits(16)?;
    Ok(())
}

fn parse_sps_hevc(data: &[u8]) -> Result<SpsHevc, PlatformError> {
    let mut bits = Bits::new(data);
    let _vps_id = bits.bits(4)?;
    let max_sub_layers = bits.bits(3)?;
    let _temporal_nesting = bits.bit()?;
    skip_profile_tier(&mut bits)?;
    let _level = bits.bits(8)?;
    let mut sub_layer_profile = vec![false; max_sub_layers as usize];
    let mut sub_layer_level = vec![false; max_sub_layers as usize];
    for index in 0..max_sub_layers as usize {
        sub_layer_profile[index] = bits.bit()? == 1;
        sub_layer_level[index] = bits.bit()? == 1;
    }
    if max_sub_layers > 0 {
        for _ in max_sub_layers..8 {
            let _ = bits.bits(2)?;
        }
    }
    for index in 0..max_sub_layers as usize {
        if sub_layer_profile[index] {
            skip_profile_tier(&mut bits)?;
        }
        if sub_layer_level[index] {
            let _ = bits.bits(8)?;
        }
    }
    let _sps_id = bits.ue()?;
    let chroma = bits.ue()?;
    if chroma == 3 {
        let _ = bits.bit()?;
    }
    let width = bits.ue()?;
    let height = bits.ue()?;
    let mut visible_w = width;
    let mut visible_h = height;
    if bits.bit()? == 1 {
        let left = bits.ue()?;
        let right = bits.ue()?;
        let top = bits.ue()?;
        let bottom = bits.ue()?;
        let unit_x = if chroma == 1 || chroma == 2 { 2 } else { 1 };
        let unit_y = if chroma == 1 { 2 } else { 1 };
        visible_w = width.saturating_sub((left + right) * unit_x);
        visible_h = height.saturating_sub((top + bottom) * unit_y);
    }
    let bit_luma = bits.ue()?;
    let bit_chroma = bits.ue()?;
    let _log2_max_poc = bits.ue()?;
    let log2_poc = _log2_max_poc;
    let _sub_layer_ordering = bits.bit()?;
    let layers = if _sub_layer_ordering == 1 { 1 } else { max_sub_layers.max(1) };
    for _ in 0..layers {
        let _ = bits.ue()?;
        let _ = bits.ue()?;
        let _ = bits.ue()?;
    }
    let log2_min_cb = bits.ue()?;
    let log2_diff_cb = bits.ue()?;
    let log2_min_tb = bits.ue()?;
    let log2_diff_tb = bits.ue()?;
    let depth_inter = bits.ue()?;
    let depth_intra = bits.ue()?;
    let _scaling = bits.bit()?;
    let amp = bits.bit()? == 1;
    let sao = bits.bit()? == 1;
    let pcm = bits.bit()? == 1;
    if pcm {
        let _ = bits.bits(4)?;
        let _ = bits.bits(4)?;
        let _ = bits.ue()?;
        let _ = bits.ue()?;
        let _ = bits.bit()?;
    }
    let num_st = bits.ue()?;
    for _ in 0..num_st {
        skip_st_ref(&mut bits)?;
    }
    let long_term = bits.bit()? == 1;
    if long_term {
        let count = bits.ue()?;
        for _ in 0..count {
            let _ = bits.bits(log2_poc.saturating_add(4))?;
            let _ = bits.bit()?;
        }
    }
    let temporal = bits.bit()? == 1;
    let strong = bits.bit()? == 1;
    let min_cb = 1u32 << log2_min_cb.saturating_add(3);
    let coded_w = width.div_ceil(min_cb.max(1)) * min_cb.max(1);
    let coded_h = height.div_ceil(min_cb.max(1)) * min_cb.max(1);
    Ok(SpsHevc {
        chroma_format_idc: chroma,
        width,
        height,
        coded_w: coded_w.max(width),
        coded_h: coded_h.max(height),
        visible_w: visible_w.max(2),
        visible_h: visible_h.max(2),
        bit_depth_luma_minus8: bit_luma,
        bit_depth_chroma_minus8: bit_chroma,
        log2_max_pic_order_cnt_lsb_minus4: log2_poc,
        log2_min_luma_coding_block_size_minus3: log2_min_cb,
        log2_diff_max_min_luma_coding_block_size: log2_diff_cb,
        log2_min_transform_block_size_minus2: log2_min_tb,
        log2_diff_max_min_transform_block_size: log2_diff_tb,
        max_transform_hierarchy_depth_inter: depth_inter,
        max_transform_hierarchy_depth_intra: depth_intra,
        sample_adaptive_offset: sao,
        temporal_mvp: temporal,
        strong_intra: strong,
        amp,
        pcm,
    })
}

fn skip_st_ref(bits: &mut Bits<'_>) -> Result<(), PlatformError> {
    let inter = bits.bit()? == 1;
    if inter {
        let _delta = bits.ue()?;
        if bits.bit()? == 1 {
            return Ok(());
        }
    }
    let neg = bits.ue()?;
    for _ in 0..neg {
        let _ = bits.ue()?;
        let _ = bits.bit()?;
    }
    let pos = bits.ue()?;
    for _ in 0..pos {
        let _ = bits.ue()?;
        let _ = bits.bit()?;
    }
    Ok(())
}

fn parse_pps_hevc(data: &[u8]) -> Result<PpsHevc, PlatformError> {
    let mut bits = Bits::new(data);
    let _pps = bits.ue()?;
    let _sps = bits.ue()?;
    let dependent = bits.bit()? == 1;
    let output = bits.bit()? == 1;
    let extra = bits.bits(3)?;
    let cabac = bits.bit()? == 1;
    let l0 = bits.ue()?;
    let l1 = bits.ue()?;
    let init_qp = bits.se()?;
    let constrained = bits.bit()? == 1;
    let transform_skip = bits.bit()? == 1;
    let cu_qp = bits.bit()? == 1;
    let mut diff_cu = 0u32;
    if cu_qp {
        diff_cu = bits.ue()?;
    }
    let cb = bits.se()?;
    let cr = bits.se()?;
    let slice_chroma = bits.bit()? == 1;
    let weighted_pred = bits.bit()? == 1;
    let weighted_bipred = bits.bit()? == 1;
    let transquant = bits.bit()? == 1;
    let tiles = bits.bit()? == 1;
    let entropy = bits.bit()? == 1;
    if tiles {
        return Err(PlatformError::new("HEVC tiles are not supported"));
    }
    let loop_filter = bits.bit()? == 1;
    let deblock_override = bits.bit()? == 1;
    let disable = bits.bit()? == 1;
    let mut beta = 0i32;
    let mut tc = 0i32;
    if !disable {
        beta = bits.se()?;
        tc = bits.se()?;
    }
    let _scaling = bits.bit()?;
    let lists = bits.bit()? == 1;
    let merge = bits.ue()?;
    let _extension = bits.bit()?;
    Ok(PpsHevc {
        dependent_slice_segments_enabled: dependent,
        output_flag_present: output,
        num_extra_slice_header_bits: extra,
        cabac_init_present: cabac,
        num_ref_idx_l0_default_active_minus1: l0,
        num_ref_idx_l1_default_active_minus1: l1,
        init_qp_minus26: init_qp,
        constrained_intra: constrained,
        transform_skip,
        cu_qp_delta_enabled: cu_qp,
        diff_cu_qp_delta_depth: diff_cu,
        pps_cb_qp_offset: cb,
        pps_cr_qp_offset: cr,
        weighted_pred,
        weighted_bipred,
        transquant_bypass: transquant,
        tiles_enabled: tiles,
        entropy_coding_sync: entropy,
        loop_filter_across_slices: loop_filter,
        deblocking_filter_override_enabled: deblock_override,
        pps_disable_deblocking: disable,
        pps_beta_offset_div2: beta,
        pps_tc_offset_div2: tc,
        lists_modification_present: lists,
        slice_chroma_qp_offsets_present: slice_chroma,
        log2_parallel_merge_level_minus2: merge,
    })
}

fn parse_slice_hevc(nal: &[u8], sps: &SpsHevc, pps: &PpsHevc) -> Result<SliceHevc, PlatformError> {
    if nal.len() < 2 {
        return Err(PlatformError::new("short HEVC NAL"));
    }
    let nal_type = hevc_nal_type(nal);
    let idr = matches!(nal_type, 19 | 20);
    let body = rbsp(&nal[2..]);
    let mut bits = Bits::new(&body);
    let first = bits.bit()? == 1;
    if !first && pps.dependent_slice_segments_enabled {
        let _dependent = bits.bit()?;
    }
    if !first {
        let min_cb = 1u32 << sps.log2_min_luma_coding_block_size_minus3.saturating_add(3);
        let ctb = min_cb << sps.log2_diff_max_min_luma_coding_block_size;
        let ctb = ctb.max(16);
        let cols = sps.width.div_ceil(ctb);
        let rows = sps.height.div_ceil(ctb);
        let bits_needed = (cols.saturating_mul(rows).max(2) as f32).log2().ceil() as u32;
        let _ = bits.bits(bits_needed)?;
    }
    let mut slice_type = 2u32;
    let mut sao_luma = false;
    let mut sao_chroma = false;
    let mut temporal = false;
    let mut qp_delta = 0i32;
    let mut deblock = pps.pps_disable_deblocking;
    let mut l0 = pps.num_ref_idx_l0_default_active_minus1;
    let mut l1 = pps.num_ref_idx_l1_default_active_minus1;
    if first || !pps.dependent_slice_segments_enabled {
        slice_type = bits.ue()?;
        if pps.output_flag_present {
            let _ = bits.bit()?;
        }
        if idr {
            // no RPS
        } else {
            let poc_bits = sps.log2_max_pic_order_cnt_lsb_minus4.saturating_add(4);
            let _poc = bits.bits(poc_bits)?;
            let short_sps = bits.bit()? == 1;
            if !short_sps {
                skip_st_ref(&mut bits)?;
            } else if bits.ue().is_err() {
                return Err(PlatformError::new("HEVC short-term RPS index"));
            }
            if sps.temporal_mvp {
                temporal = bits.bit()? == 1;
            }
        }
        if sps.sample_adaptive_offset {
            sao_luma = bits.bit()? == 1;
            if sps.chroma_format_idc != 0 {
                sao_chroma = bits.bit()? == 1;
            }
        }
        if slice_type == 0 || slice_type == 1 {
            if bits.bit()? == 1 {
                l0 = bits.ue()?;
                if slice_type == 0 {
                    l1 = bits.ue()?;
                }
            }
        }
        if pps.cabac_init_present && slice_type != 2 {
            let _ = bits.bit()?;
        }
        qp_delta = bits.se()?;
        if pps.deblocking_filter_override_enabled && bits.bit()? == 1 {
            deblock = bits.bit()? == 1;
            if !deblock {
                let _ = bits.se()?;
                let _ = bits.se()?;
            }
        }
    }
    let poc = if idr { 0 } else { bits.used() as i32 };
    let header_bytes = (8 + bits.used() as u32).div_ceil(8);
    Ok(SliceHevc {
        idr,
        slice_type,
        poc,
        slice_qp_delta: qp_delta,
        sao_luma,
        sao_chroma,
        temporal_mvp: temporal,
        deblocking_disabled: deblock,
        header_bytes,
        num_ref_idx_l0: l0,
        num_ref_idx_l1: l1,
    })
}

fn picture_hevc(surface: u32, sps: &SpsHevc, pps: &PpsHevc, header: &SliceHevc, dpb: &[RefHevc]) -> DecPictureHevc {
    let mut pic = unsafe { std::mem::zeroed::<DecPictureHevc>() };
    pic.curr_pic = PictureHevc { picture_id: surface, pic_order_cnt: header.poc, flags: 0, va_reserved: [0; 4] };
    for slot in &mut pic.reference_frames {
        *slot = PictureHevc { picture_id: VA_INVALID_ID, pic_order_cnt: 0, flags: 1, va_reserved: [0; 4] };
    }
    for (slot, reference) in pic.reference_frames.iter_mut().zip(dpb.iter().rev()) {
        *slot = PictureHevc {
            picture_id: reference.surface,
            pic_order_cnt: reference.poc,
            flags: 0x10,
            va_reserved: [0; 4],
        };
    }
    pic.pic_width_in_luma_samples = sps.width as u16;
    pic.pic_height_in_luma_samples = sps.height as u16;
    let mut fields = sps.chroma_format_idc & 3;
    fields |= u32::from(sps.pcm) << 3;
    fields |= u32::from(sps.amp) << 6;
    fields |= u32::from(sps.strong_intra) << 7;
    fields |= u32::from(pps.constrained_intra) << 9;
    fields |= u32::from(pps.cu_qp_delta_enabled) << 10;
    fields |= u32::from(pps.weighted_pred) << 11;
    fields |= u32::from(pps.weighted_bipred) << 12;
    fields |= u32::from(pps.transquant_bypass) << 13;
    fields |= u32::from(pps.tiles_enabled) << 14;
    fields |= u32::from(pps.entropy_coding_sync) << 15;
    fields |= u32::from(pps.loop_filter_across_slices) << 16;
    pic.pic_fields = fields;
    pic.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
    pic.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
    pic.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3 as u8;
    pic.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size as u8;
    pic.log2_min_transform_block_size_minus2 = sps.log2_min_transform_block_size_minus2 as u8;
    pic.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_transform_block_size as u8;
    pic.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
    pic.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
    pic.init_qp_minus26 = pps.init_qp_minus26 as i8;
    pic.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth as u8;
    pic.pps_cb_qp_offset = pps.pps_cb_qp_offset as i8;
    pic.pps_cr_qp_offset = pps.pps_cr_qp_offset as i8;
    pic.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2 as u8;
    pic.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4 as u8;
    pic.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1 as u8;
    pic.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1 as u8;
    pic.pps_beta_offset_div2 = pps.pps_beta_offset_div2 as i8;
    pic.pps_tc_offset_div2 = pps.pps_tc_offset_div2 as i8;
    pic.num_extra_slice_header_bits = pps.num_extra_slice_header_bits as u8;
    let mut parsing = u32::from(pps.lists_modification_present);
    parsing |= u32::from(sps.temporal_mvp) << 2;
    parsing |= u32::from(pps.cabac_init_present) << 3;
    parsing |= u32::from(pps.output_flag_present) << 4;
    parsing |= u32::from(pps.dependent_slice_segments_enabled) << 5;
    parsing |= u32::from(pps.slice_chroma_qp_offsets_present) << 6;
    parsing |= u32::from(sps.sample_adaptive_offset) << 7;
    parsing |= u32::from(pps.deblocking_filter_override_enabled) << 8;
    parsing |= u32::from(pps.pps_disable_deblocking) << 9;
    parsing |= u32::from(header.idr) << 12;
    parsing |= u32::from(header.slice_type == 2) << 13;
    pic.slice_parsing_fields = parsing;
    pic
}

fn slice_param_hevc(nal: &[u8], header: &SliceHevc, dpb: &[RefHevc], last: bool) -> DecSliceHevc {
    let mut slice = unsafe { std::mem::zeroed::<DecSliceHevc>() };
    slice.slice_data_size = nal.len() as u32;
    slice.slice_data_byte_offset = header.header_bytes;
    slice.ref_pic_list = [[0xff; 15]; 2];
    if let Some(_) = dpb.last() {
        slice.ref_pic_list[0][0] = 0;
    }
    let mut flags = u32::from(last);
    flags |= (header.slice_type & 3) << 2;
    flags |= u32::from(header.sao_luma) << 6;
    flags |= u32::from(header.sao_chroma) << 7;
    flags |= u32::from(header.temporal_mvp) << 10;
    flags |= u32::from(header.deblocking_disabled) << 11;
    slice.long_slice_flags = flags;
    slice.num_ref_idx_l0_active_minus1 = header.num_ref_idx_l0 as u8;
    slice.num_ref_idx_l1_active_minus1 = header.num_ref_idx_l1 as u8;
    slice.slice_qp_delta = header.slice_qp_delta as i8;
    slice.collocated_ref_idx = 0xff;
    slice.five_minus_max_num_merge_cand = 0;
    slice
}

#[repr(C)]
struct DecPictureH264 {
    curr_pic: PictureH264,
    reference_frames: [PictureH264; 16],
    picture_width_in_mbs_minus1: u16,
    picture_height_in_mbs_minus1: u16,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames: u8,
    seq_fields: u32,
    num_slice_groups_minus1: u8,
    slice_group_map_type: u8,
    slice_group_change_rate_minus1: u16,
    pic_init_qp_minus26: i8,
    pic_init_qs_minus26: i8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    frame_num: u16,
    va_reserved: [u32; 8],
}

#[repr(C)]
struct DecSliceH264 {
    slice_data_size: u32,
    slice_data_offset: u32,
    slice_data_flag: u32,
    slice_data_bit_offset: u16,
    first_mb_in_slice: u16,
    slice_type: u8,
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    ref_pic_list0: [PictureH264; 32],
    ref_pic_list1: [PictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    va_reserved: [u32; 4],
}

#[repr(C)]
struct DecPictureHevc {
    curr_pic: PictureHevc,
    reference_frames: [PictureHevc; 15],
    pic_width_in_luma_samples: u16,
    pic_height_in_luma_samples: u16,
    pic_fields: u32,
    sps_max_dec_pic_buffering_minus1: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    pcm_sample_bit_depth_luma_minus1: u8,
    pcm_sample_bit_depth_chroma_minus1: u8,
    log2_min_luma_coding_block_size_minus3: u8,
    log2_diff_max_min_luma_coding_block_size: u8,
    log2_min_transform_block_size_minus2: u8,
    log2_diff_max_min_transform_block_size: u8,
    log2_min_pcm_luma_coding_block_size_minus3: u8,
    log2_diff_max_min_pcm_luma_coding_block_size: u8,
    max_transform_hierarchy_depth_intra: u8,
    max_transform_hierarchy_depth_inter: u8,
    init_qp_minus26: i8,
    diff_cu_qp_delta_depth: u8,
    pps_cb_qp_offset: i8,
    pps_cr_qp_offset: i8,
    log2_parallel_merge_level_minus2: u8,
    num_tile_columns_minus1: u8,
    num_tile_rows_minus1: u8,
    column_width_minus1: [u16; 19],
    row_height_minus1: [u16; 21],
    slice_parsing_fields: u32,
    log2_max_pic_order_cnt_lsb_minus4: u8,
    num_short_term_ref_pic_sets: u8,
    num_long_term_ref_pic_sps: u8,
    num_ref_idx_l0_default_active_minus1: u8,
    num_ref_idx_l1_default_active_minus1: u8,
    pps_beta_offset_div2: i8,
    pps_tc_offset_div2: i8,
    num_extra_slice_header_bits: u8,
    st_rps_bits: u32,
    va_reserved: [u32; 8],
}

#[repr(C)]
struct DecSliceHevc {
    slice_data_size: u32,
    slice_data_offset: u32,
    slice_data_flag: u32,
    slice_data_byte_offset: u32,
    slice_segment_address: u32,
    ref_pic_list: [[u8; 15]; 2],
    long_slice_flags: u32,
    collocated_ref_idx: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    slice_qp_delta: i8,
    slice_cb_qp_offset: i8,
    slice_cr_qp_offset: i8,
    slice_beta_offset_div2: i8,
    slice_tc_offset_div2: i8,
    luma_log2_weight_denom: u8,
    delta_chroma_log2_weight_denom: i8,
    delta_luma_weight_l0: [i8; 15],
    luma_offset_l0: [i8; 15],
    delta_chroma_weight_l0: [[i8; 2]; 15],
    chroma_offset_l0: [[i8; 2]; 15],
    delta_luma_weight_l1: [i8; 15],
    luma_offset_l1: [i8; 15],
    delta_chroma_weight_l1: [[i8; 2]; 15],
    chroma_offset_l1: [[i8; 2]; 15],
    five_minus_max_num_merge_cand: u8,
    num_entry_point_offsets: u16,
    entry_offset_to_subset_array: u16,
    slice_data_num_emu_prevn_bytes: u16,
    va_reserved: [u32; 2],
}
