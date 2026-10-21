//! VA-API EncSlice encoder. Output is AVCC (4-byte big-endian NAL lengths).

use std::time::Instant;

use deskbridge_proto::{CodecConfig, CodecId, VideoFrame};

use crate::codec::{
    align_up, annex_b_nals, avcc, bgra_to_nv12, h264_nal_type, hevc_nal_type, image_format, PictureH264,
    PictureHevc, VaApi, BUF_ENC_CODED, BUF_ENC_MISC, BUF_ENC_PICTURE, BUF_ENC_SEQUENCE, BUF_ENC_SLICE,
    CONFIG_RATE_CONTROL, CONFIG_RT_FORMAT, CodedSegment, MISC_FRAME_RATE, MISC_RATE_CONTROL,
    PICTURE_H264_INVALID, PICTURE_H264_SHORT_TERM, PICTURE_HEVC_INVALID, VA_FOURCC_NV12, VA_INVALID_ID,
    VA_PROGRESSIVE, VA_RC_CBR, VA_RT_FORMAT_YUV420,
};
use crate::error::PlatformError;

const PROFILE_H264_MAIN: i32 = 6;
const PROFILE_H264_HIGH: i32 = 7;
const PROFILE_HEVC_MAIN: i32 = 17;
const ENTRY_ENC_SLICE: i32 = 6;

pub struct Encoded {
    pub frame: VideoFrame,
    pub config: Option<CodecConfig>,
}

pub struct Encoder {
    api: VaApi,
    codec: CodecId,
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    fps: u32,
    target_fps: u32,
    bitrate: u32,
    next_id: u64,
    frame_num: u32,
    poc: i32,
    idr_pic_id: u16,
    config_id: u32,
    context: u32,
    surfaces: [u32; 2],
    which: usize,
    have_ref: bool,
    profile: i32,
    rate_control: u32,
}

impl Encoder {
    pub fn open(codec: CodecId, width: u32, height: u32, bitrate: u32, fps: u32) -> Result<Self, PlatformError> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
            return Err(PlatformError::new("encoder dimensions must be even"));
        }
        let api = VaApi::open()?;
        let profile = pick_profile(&api, codec)?;
        let rate_control = api.rate_control(profile, ENTRY_ENC_SLICE);
        let (coded_w, coded_h) = coded_size(codec, width, height);
        let (config_id, context, surfaces) = build_context(&api, profile, rate_control, coded_w, coded_h)?;
        Ok(Self {
            api,
            codec,
            width,
            height,
            coded_w,
            coded_h,
            fps: fps.clamp(1, 240),
            target_fps: fps.clamp(1, 60),
            bitrate: bitrate.max(1),
            next_id: 1,
            frame_num: 0,
            poc: 0,
            idr_pic_id: 0,
            config_id,
            context,
            surfaces,
            which: 0,
            have_ref: false,
            profile,
            rate_control,
        })
    }

    pub fn set_target_fps(&mut self, fps: u32) -> bool {
        let fps = fps.clamp(1, 60);
        self.target_fps = fps;
        true
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> bool {
        self.bitrate = bitrate.max(1);
        true
    }

    pub fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        capture_us: u64,
        force_key: bool,
    ) -> Result<Option<Encoded>, PlatformError> {
        if width != self.width || height != self.height {
            self.rebuild(width, height)?;
        }
        if bgra.len() < width as usize * height as usize * 4 {
            return Err(PlatformError::new("frame is smaller than its dimensions"));
        }
        let started = Instant::now();
        let idr = force_key || !self.have_ref;
        let current = self.surfaces[self.which];
        let previous = self.surfaces[1 - self.which];
        self.upload(current, bgra, width, height)?;
        let mut buffers = BufferList { api: &self.api, ids: Vec::new() };
        let coded_id = self.make_coded(&mut buffers)?;
        self.push_sequence(&mut buffers)?;
        self.push_misc(&mut buffers)?;
        match self.codec {
            CodecId::H264 => self.render_h264(&mut buffers, current, previous, coded_id, idr)?,
            CodecId::Hevc => self.render_hevc(&mut buffers, current, previous, coded_id, idr)?,
        }
        let display = self.api.display();
        self.api.check(unsafe { (self.api.begin_picture)(display, self.context, current) }, "vaBeginPicture")?;
        if !buffers.ids.is_empty() {
            self.api.check(
                unsafe { (self.api.render_picture)(display, self.context, buffers.ids.as_mut_ptr(), buffers.ids.len() as i32) },
                "vaRenderPicture",
            )?;
        }
        self.api.check(unsafe { (self.api.end_picture)(display, self.context) }, "vaEndPicture")?;
        self.api.check(unsafe { (self.api.sync_surface)(display, current) }, "vaSyncSurface")?;
        let bytes = read_coded(&self.api, coded_id)?;
        drop(buffers);
        if bytes.is_empty() {
            return Ok(None);
        }
        let nals = annex_b_nals(&bytes);
        let (sets, slices, keyframe) = split_nals(self.codec, &nals, idr);
        if slices.is_empty() && sets.is_empty() {
            return Ok(None);
        }
        let payload = avcc(if slices.is_empty() { &sets } else { &slices });
        if payload.is_empty() {
            return Ok(None);
        }
        let config = if keyframe && !sets.is_empty() {
            Some(CodecConfig {
                codec: self.codec,
                width: self.width,
                height: self.height,
                nal_length_size: 4,
                parameter_sets: sets,
            })
        } else {
            None
        };
        let encode_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.frame_num = self.frame_num.wrapping_add(1) & 0xff;
        self.poc = self.poc.wrapping_add(2);
        if idr {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1) & 0xff;
        }
        self.have_ref = true;
        self.which = 1 - self.which;
        Ok(Some(Encoded {
            frame: VideoFrame {
                id,
                keyframe,
                codec: self.codec,
                capture_us,
                encode_us,
                payload,
            },
            config,
        }))
    }

    fn rebuild(&mut self, width: u32, height: u32) -> Result<(), PlatformError> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
            return Err(PlatformError::new("encoder dimensions must be even"));
        }
        self.destroy_context();
        let (coded_w, coded_h) = coded_size(self.codec, width, height);
        let (config_id, context, surfaces) = build_context(&self.api, self.profile, self.rate_control, coded_w, coded_h)?;
        self.width = width;
        self.height = height;
        self.coded_w = coded_w;
        self.coded_h = coded_h;
        self.config_id = config_id;
        self.context = context;
        self.surfaces = surfaces;
        self.which = 0;
        self.have_ref = false;
        self.frame_num = 0;
        self.poc = 0;
        Ok(())
    }

    fn destroy_context(&mut self) {
        let display = self.api.display();
        if self.context != VA_INVALID_ID {
            unsafe { (self.api.destroy_context)(display, self.context) };
            self.context = VA_INVALID_ID;
        }
        unsafe { (self.api.destroy_surfaces)(display, self.surfaces.as_mut_ptr(), 2) };
        self.surfaces = [VA_INVALID_ID; 2];
        if self.config_id != VA_INVALID_ID {
            unsafe { (self.api.destroy_config)(display, self.config_id) };
            self.config_id = VA_INVALID_ID;
        }
    }

    fn upload(&self, surface: u32, bgra: &[u8], width: u32, height: u32) -> Result<(), PlatformError> {
        let mut format = image_format(VA_FOURCC_NV12, 12);
        let mut image = zeroed_image();
        let display = self.api.display();
        self.api.check(
            unsafe { (self.api.create_image)(display, &mut format, self.coded_w as i32, self.coded_h as i32, &mut image) },
            "vaCreateImage",
        )?;
        let mut mapped = std::ptr::null_mut();
        let mapped_ok = self.api.check(unsafe { (self.api.map_buffer)(display, image.buf, &mut mapped) }, "vaMapBuffer");
        if let Err(err) = mapped_ok {
            unsafe { (self.api.destroy_image)(display, image.image_id) };
            return Err(err);
        }
        let size = image.data_size.max(1) as usize;
        unsafe {
            let dest = std::slice::from_raw_parts_mut(mapped.cast::<u8>(), size);
            dest.fill(0);
            bgra_to_nv12(bgra, width, height, self.coded_w, self.coded_h, image.pitches, image.offsets, dest);
        }
        let _ = self.api.check(unsafe { (self.api.unmap_buffer)(display, image.buf) }, "vaUnmapBuffer");
        let put = self.api.check(
            unsafe {
                (self.api.put_image)(
                    display,
                    surface,
                    image.image_id,
                    0,
                    0,
                    self.coded_w,
                    self.coded_h,
                    0,
                    0,
                    self.coded_w,
                    self.coded_h,
                )
            },
            "vaPutImage",
        );
        unsafe { (self.api.destroy_image)(display, image.image_id) };
        put
    }

    fn make_coded(&self, buffers: &mut BufferList<'_>) -> Result<u32, PlatformError> {
        let mut id = 0u32;
        let size = (self.coded_w as usize * self.coded_h as usize).max(64 * 1024) as u32;
        self.api.check(
            unsafe {
                (self.api.create_buffer)(
                    self.api.display(),
                    self.context,
                    BUF_ENC_CODED,
                    size,
                    1,
                    std::ptr::null_mut(),
                    &mut id,
                )
            },
            "vaCreateBuffer",
        )?;
        buffers.ids.push(id);
        Ok(id)
    }

    fn make_buffer<T>(&self, kind: i32, value: &T, buffers: &mut BufferList<'_>) -> Result<u32, PlatformError> {
        let mut id = 0u32;
        self.api.check(
            unsafe {
                (self.api.create_buffer)(
                    self.api.display(),
                    self.context,
                    kind,
                    std::mem::size_of::<T>() as u32,
                    1,
                    (value as *const T).cast_mut().cast(),
                    &mut id,
                )
            },
            "vaCreateBuffer",
        )?;
        buffers.ids.push(id);
        Ok(id)
    }

    fn push_sequence(&self, buffers: &mut BufferList<'_>) -> Result<(), PlatformError> {
        match self.codec {
            CodecId::H264 => {
                let seq = h264_sequence(self.width, self.height, self.coded_w, self.coded_h, self.bitrate, self.target_fps);
                self.make_buffer(BUF_ENC_SEQUENCE, &seq, buffers)?;
            }
            CodecId::Hevc => {
                let seq = hevc_sequence(self.width, self.height, self.bitrate, self.target_fps);
                self.make_buffer(BUF_ENC_SEQUENCE, &seq, buffers)?;
            }
        }
        Ok(())
    }

    fn push_misc(&self, buffers: &mut BufferList<'_>) -> Result<(), PlatformError> {
        let rate = MiscRateControl {
            kind: MISC_RATE_CONTROL,
            bits_per_second: self.bitrate,
            target_percentage: 100,
            window_size: 1_000,
            initial_qp: 0,
            min_qp: 0,
            basic_unit_size: 0,
            rc_flags: 0x2,
            icq: 0,
            max_qp: 0,
            quality_factor: 0,
            target_frame_size: 0,
            reserved: [0; 4],
        };
        self.make_buffer(BUF_ENC_MISC, &rate, buffers)?;
        let fps = MiscFrameRate {
            kind: MISC_FRAME_RATE,
            framerate: self.target_fps.max(1),
            flags: 0,
            reserved: [0; 4],
        };
        self.make_buffer(BUF_ENC_MISC, &fps, buffers)?;
        Ok(())
    }

    fn render_h264(&self, buffers: &mut BufferList<'_>, current: u32, previous: u32, coded: u32, idr: bool) -> Result<(), PlatformError> {
        let mut pic = h264_picture(current, coded, self.frame_num, self.poc, idr, self.profile == PROFILE_H264_HIGH);
        if !idr && self.have_ref {
            pic.reference_frames[0] = PictureH264 {
                picture_id: previous,
                frame_idx: self.frame_num.wrapping_sub(1) & 0xff,
                flags: PICTURE_H264_SHORT_TERM,
                top_field_order_cnt: self.poc.wrapping_sub(2),
                bottom_field_order_cnt: self.poc.wrapping_sub(2),
                va_reserved: [0; 4],
            };
        }
        self.make_buffer(BUF_ENC_PICTURE, &pic, buffers)?;
        let slice = h264_slice(self.coded_w, self.coded_h, previous, self.poc, self.idr_pic_id, idr && self.have_ref == false || idr, idr, self.have_ref);
        let _ = (idr,);
        self.make_buffer(BUF_ENC_SLICE, &slice, buffers)?;
        Ok(())
    }

    fn render_hevc(&self, buffers: &mut BufferList<'_>, current: u32, previous: u32, coded: u32, idr: bool) -> Result<(), PlatformError> {
        let pic = hevc_picture(current, previous, coded, self.poc, idr, self.have_ref);
        self.make_buffer(BUF_ENC_PICTURE, &pic, buffers)?;
        let slice = hevc_slice(self.coded_w, self.coded_h, previous, self.poc, idr, self.have_ref);
        self.make_buffer(BUF_ENC_SLICE, &slice, buffers)?;
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.destroy_context();
    }
}

struct BufferList<'a> {
    api: &'a VaApi,
    ids: Vec<u32>,
}

impl Drop for BufferList<'_> {
    fn drop(&mut self) {
        for id in &self.ids {
            unsafe { (self.api.destroy_buffer)(self.api.display(), *id) };
        }
    }
}

fn pick_profile(api: &VaApi, codec: CodecId) -> Result<i32, PlatformError> {
    let profiles = api.profiles()?;
    let wanted: &[i32] = match codec {
        CodecId::Hevc => &[PROFILE_HEVC_MAIN],
        CodecId::H264 => &[PROFILE_H264_HIGH, PROFILE_H264_MAIN],
    };
    wanted
        .iter()
        .copied()
        .find(|profile| profiles.contains(profile) && api.supports(*profile, ENTRY_ENC_SLICE))
        .ok_or_else(|| PlatformError::new(format!("no hardware {} encoder", codec.name())))
}

fn coded_size(codec: CodecId, width: u32, height: u32) -> (u32, u32) {
    match codec {
        CodecId::H264 => (align_up(width, 16), align_up(height, 16)),
        CodecId::Hevc => (align_up(width, 8), align_up(height, 8)),
    }
}

fn build_context(api: &VaApi, profile: i32, rate_control: u32, width: u32, height: u32) -> Result<(u32, u32, [u32; 2]), PlatformError> {
    let mut attribs = [
        crate::codec::ConfigAttrib { kind: CONFIG_RT_FORMAT, value: VA_RT_FORMAT_YUV420 },
        crate::codec::ConfigAttrib { kind: CONFIG_RATE_CONTROL, value: rate_control },
    ];
    let mut config = 0u32;
    api.check(
        unsafe { (api.create_config)(api.display(), profile, ENTRY_ENC_SLICE, attribs.as_mut_ptr(), 2, &mut config) },
        "vaCreateConfig",
    )?;
    let mut surfaces = [0u32; 2];
    let surfaces_ok = api.check(
        unsafe {
            (api.create_surfaces)(
                api.display(),
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                2,
                std::ptr::null_mut(),
                0,
            )
        },
        "vaCreateSurfaces",
    );
    if let Err(err) = surfaces_ok {
        unsafe { (api.destroy_config)(api.display(), config) };
        return Err(err);
    }
    let mut context = 0u32;
    let context_ok = api.check(
        unsafe {
            (api.create_context)(
                api.display(),
                config,
                width as i32,
                height as i32,
                VA_PROGRESSIVE,
                surfaces.as_mut_ptr(),
                2,
                &mut context,
            )
        },
        "vaCreateContext",
    );
    if let Err(err) = context_ok {
        unsafe { (api.destroy_surfaces)(api.display(), surfaces.as_mut_ptr(), 2) };
        unsafe { (api.destroy_config)(api.display(), config) };
        return Err(err);
    }
    let _ = VA_RC_CBR;
    Ok((config, context, surfaces))
}

fn zeroed_image() -> crate::codec::Image {
    unsafe { std::mem::zeroed() }
}

fn read_coded(api: &VaApi, buf: u32) -> Result<Vec<u8>, PlatformError> {
    let mut ptr = std::ptr::null_mut();
    api.check(unsafe { (api.map_buffer)(api.display(), buf, &mut ptr) }, "vaMapBuffer")?;
    let mut out = Vec::new();
    unsafe {
        let mut seg = ptr.cast::<CodedSegment>();
        let mut guard = 0;
        while !seg.is_null() && guard < 64 {
            let segment = &*seg;
            let size = segment.size as usize;
            if !segment.buf.is_null() && size > 0 {
                let bytes = std::slice::from_raw_parts(segment.buf.cast::<u8>(), size);
                let start = (segment.bit_offset as usize / 8).min(bytes.len());
                out.extend_from_slice(&bytes[start..]);
            }
            seg = segment.next;
            guard += 1;
        }
    }
    api.check(unsafe { (api.unmap_buffer)(api.display(), buf) }, "vaUnmapBuffer")?;
    Ok(out)
}

fn split_nals(codec: CodecId, nals: &[Vec<u8>], requested_idr: bool) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, bool) {
    let mut sets = Vec::new();
    let mut slices = Vec::new();
    let mut idr = false;
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let kind = match codec {
            CodecId::H264 => h264_nal_type(nal),
            CodecId::Hevc => hevc_nal_type(nal),
        };
        let parameter = match codec {
            CodecId::H264 => matches!(kind, 7 | 8),
            CodecId::Hevc => matches!(kind, 32 | 33 | 34),
        };
        let slice = match codec {
            CodecId::H264 => matches!(kind, 1 | 5),
            CodecId::Hevc => kind <= 31,
        };
        if codec == CodecId::H264 && kind == 5 {
            idr = true;
        }
        if codec == CodecId::Hevc && matches!(kind, 19 | 20 | 21) {
            idr = true;
        }
        if parameter {
            sets.push(nal.clone());
        } else if slice {
            slices.push(nal.clone());
        }
    }
    (sets, slices, idr || requested_idr)
}

fn level_h264(width: u32, height: u32) -> u8 {
    if width <= 1280 && height <= 720 {
        31
    } else if width <= 1920 && height <= 1088 {
        40
    } else {
        51
    }
}

fn h264_sequence(width: u32, height: u32, coded_w: u32, coded_h: u32, bitrate: u32, fps: u32) -> EncSequenceH264 {
    let mut seq = zeroed::<EncSequenceH264>();
    seq.seq_parameter_set_id = 0;
    seq.level_idc = level_h264(coded_w, coded_h);
    seq.intra_period = 3600;
    seq.intra_idr_period = 3600;
    seq.ip_period = 1;
    seq.bits_per_second = bitrate;
    seq.max_num_ref_frames = 1;
    seq.picture_width_in_mbs = (coded_w / 16) as u16;
    seq.picture_height_in_mbs = (coded_h / 16) as u16;
    // chroma 4:2:0, frame_mbs_only, direct_8x8, log2_max_frame_num_minus4 = 4, poc type 0, log2 poc lsb minus4 = 4
    seq.seq_fields = 1 | (1 << 2) | (1 << 5) | (4 << 6) | (4 << 12);
    let crop_right = coded_w.saturating_sub(width) / 2;
    let crop_bottom = coded_h.saturating_sub(height) / 2;
    if crop_right > 0 || crop_bottom > 0 {
        seq.frame_cropping_flag = 1;
        seq.frame_crop_right_offset = crop_right;
        seq.frame_crop_bottom_offset = crop_bottom;
    }
    seq.vui_parameters_present_flag = 1;
    // timing_info_present + fixed_frame_rate
    seq.vui_fields = (1 << 1) | (1 << 13);
    seq.num_units_in_tick = 1;
    seq.time_scale = fps.max(1).saturating_mul(2);
    seq
}

fn invalid_h264() -> PictureH264 {
    PictureH264 {
        picture_id: VA_INVALID_ID,
        frame_idx: 0,
        flags: PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    }
}

fn h264_picture(current: u32, coded: u32, frame_num: u32, poc: i32, idr: bool, high: bool) -> EncPictureH264 {
    let mut pic = zeroed::<EncPictureH264>();
    pic.curr_pic = PictureH264 {
        picture_id: current,
        frame_idx: frame_num & 0xff,
        flags: 0,
        top_field_order_cnt: poc,
        bottom_field_order_cnt: poc,
        va_reserved: [0; 4],
    };
    pic.reference_frames = [invalid_h264(); 16];
    pic.coded_buf = coded;
    pic.pic_parameter_set_id = 0;
    pic.seq_parameter_set_id = 0;
    pic.frame_num = (frame_num & 0xff) as u16;
    pic.pic_init_qp = 26;
    // idr, reference (bit 1), cabac (bit 3), deblocking present (bit 9), 8x8 for high (bit 8)
    let mut fields = u32::from(idr) | (1 << 1) | (1 << 3) | (1 << 9);
    if high {
        fields |= 1 << 8;
    }
    pic.pic_fields = fields;
    pic
}

fn h264_slice(coded_w: u32, coded_h: u32, previous: u32, poc: i32, idr_pic_id: u16, _force: bool, idr: bool, have_ref: bool) -> EncSliceH264 {
    let mut slice = zeroed::<EncSliceH264>();
    slice.macroblock_address = 0;
    slice.num_macroblocks = (coded_w / 16) * (coded_h / 16);
    slice.macroblock_info = VA_INVALID_ID;
    slice.slice_type = if idr || !have_ref { 2 } else { 0 };
    slice.idr_pic_id = idr_pic_id;
    slice.pic_order_cnt_lsb = (poc as u16) & 0xff;
    slice.ref_pic_list0 = [invalid_h264(); 32];
    slice.ref_pic_list1 = [invalid_h264(); 32];
    if !idr && have_ref {
        slice.ref_pic_list0[0] = PictureH264 {
            picture_id: previous,
            frame_idx: 0,
            flags: PICTURE_H264_SHORT_TERM,
            top_field_order_cnt: poc.wrapping_sub(2),
            bottom_field_order_cnt: poc.wrapping_sub(2),
            va_reserved: [0; 4],
        };
    }
    slice.disable_deblocking_filter_idc = 0;
    slice
}

fn invalid_hevc() -> PictureHevc {
    PictureHevc {
        picture_id: VA_INVALID_ID,
        pic_order_cnt: 0,
        flags: PICTURE_HEVC_INVALID,
        va_reserved: [0; 4],
    }
}

fn hevc_sequence(width: u32, height: u32, bitrate: u32, fps: u32) -> EncSequenceHevc {
    let mut seq = zeroed::<EncSequenceHevc>();
    seq.general_profile_idc = 1;
    seq.general_level_idc = if width <= 1280 && height <= 720 {
        93
    } else if width <= 1920 && height <= 1088 {
        123
    } else {
        153
    };
    seq.intra_period = 3600;
    seq.intra_idr_period = 3600;
    seq.ip_period = 1;
    seq.bits_per_second = bitrate;
    seq.pic_width_in_luma_samples = width as u16;
    seq.pic_height_in_luma_samples = height as u16;
    // chroma 4:2:0, strong intra smoothing, low delay
    seq.seq_fields = 1 | (1 << 10) | (1 << 16);
    seq.log2_diff_max_min_luma_coding_block_size = 3;
    seq.log2_diff_max_min_transform_block_size = 3;
    seq.max_transform_hierarchy_depth_inter = 2;
    seq.max_transform_hierarchy_depth_intra = 2;
    seq.vui_parameters_present_flag = 1;
    seq.vui_fields = 1 << 3;
    seq.vui_num_units_in_tick = 1;
    seq.vui_time_scale = fps.max(1);
    seq
}

fn hevc_picture(current: u32, previous: u32, coded: u32, poc: i32, idr: bool, have_ref: bool) -> EncPictureHevc {
    let mut pic = zeroed::<EncPictureHevc>();
    pic.decoded_curr_pic = PictureHevc {
        picture_id: current,
        pic_order_cnt: poc,
        flags: 0,
        va_reserved: [0; 4],
    };
    pic.reference_frames = [invalid_hevc(); 15];
    if !idr && have_ref {
        pic.reference_frames[0] = PictureHevc {
            picture_id: previous,
            pic_order_cnt: poc.wrapping_sub(2),
            flags: 0,
            va_reserved: [0; 4],
        };
    }
    pic.coded_buf = coded;
    pic.collocated_ref_pic_index = 0xff;
    pic.pic_init_qp = 26;
    pic.nal_unit_type = if idr || !have_ref { 19 } else { 1 };
    let coding = if idr || !have_ref { 1u32 } else { 2 };
    // idr bit 0, coding_type bits 1-3, reference bit 4, loop filter across slices bit 16
    pic.pic_fields = u32::from(idr) | (coding << 1) | (1 << 4) | (1 << 16);
    pic
}

fn hevc_slice(coded_w: u32, coded_h: u32, previous: u32, poc: i32, idr: bool, have_ref: bool) -> EncSliceHevc {
    let mut slice = zeroed::<EncSliceHevc>();
    let ctu = coded_w.div_ceil(64) * coded_h.div_ceil(64);
    slice.num_ctu_in_slice = ctu.max(1);
    slice.slice_type = if idr || !have_ref { 2 } else { 1 };
    slice.ref_pic_list0 = [invalid_hevc(); 15];
    slice.ref_pic_list1 = [invalid_hevc(); 15];
    if !idr && have_ref {
        slice.ref_pic_list0[0] = PictureHevc {
            picture_id: previous,
            pic_order_cnt: poc.wrapping_sub(2),
            flags: 0,
            va_reserved: [0; 4],
        };
    }
    slice.max_num_merge_cand = 5;
    // last slice, loop filter across slices, collocated from l0
    slice.slice_fields = 1 | (1 << 12) | (1 << 13);
    slice
}

fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

#[repr(C)]
struct EncSequenceH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct EncPictureH264 {
    curr_pic: PictureH264,
    reference_frames: [PictureH264; 16],
    coded_buf: u32,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct EncSliceH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: u32,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
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
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct EncSequenceHevc {
    general_profile_idc: u8,
    general_level_idc: u8,
    general_tier_flag: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    pic_width_in_luma_samples: u16,
    pic_height_in_luma_samples: u16,
    seq_fields: u32,
    log2_min_luma_coding_block_size_minus3: u8,
    log2_diff_max_min_luma_coding_block_size: u8,
    log2_min_transform_block_size_minus2: u8,
    log2_diff_max_min_transform_block_size: u8,
    max_transform_hierarchy_depth_inter: u8,
    max_transform_hierarchy_depth_intra: u8,
    pcm_sample_bit_depth_luma_minus1: u32,
    pcm_sample_bit_depth_chroma_minus1: u32,
    log2_min_pcm_luma_coding_block_size_minus3: u32,
    log2_max_pcm_luma_coding_block_size_minus3: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    vui_num_units_in_tick: u32,
    vui_time_scale: u32,
    min_spatial_segmentation_idc: u16,
    max_bytes_per_pic_denom: u8,
    max_bits_per_min_cu_denom: u8,
    scc_fields: u32,
    va_reserved: [u32; 7],
}

#[repr(C)]
struct EncPictureHevc {
    decoded_curr_pic: PictureHevc,
    reference_frames: [PictureHevc; 15],
    coded_buf: u32,
    collocated_ref_pic_index: u8,
    last_picture: u8,
    pic_init_qp: u8,
    diff_cu_qp_delta_depth: u8,
    pps_cb_qp_offset: i8,
    pps_cr_qp_offset: i8,
    num_tile_columns_minus1: u8,
    num_tile_rows_minus1: u8,
    column_width_minus1: [u8; 19],
    row_height_minus1: [u8; 21],
    log2_parallel_merge_level_minus2: u8,
    ctu_max_bitsize_allowed: u8,
    num_ref_idx_l0_default_active_minus1: u8,
    num_ref_idx_l1_default_active_minus1: u8,
    slice_pic_parameter_set_id: u8,
    nal_unit_type: u8,
    pic_fields: u32,
    hierarchical_level_plus1: u8,
    va_byte_reserved: u8,
    scc_fields: u16,
    va_reserved: [u32; 15],
}

#[repr(C)]
struct EncSliceHevc {
    slice_segment_address: u32,
    num_ctu_in_slice: u32,
    slice_type: u8,
    slice_pic_parameter_set_id: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [PictureHevc; 15],
    ref_pic_list1: [PictureHevc; 15],
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
    max_num_merge_cand: u8,
    slice_qp_delta: i8,
    slice_cb_qp_offset: i8,
    slice_cr_qp_offset: i8,
    slice_beta_offset_div2: i8,
    slice_tc_offset_div2: i8,
    slice_fields: u32,
    pred_weight_table_bit_offset: u32,
    pred_weight_table_bit_length: u32,
    va_reserved: [u32; 6],
}

#[repr(C)]
struct MiscRateControl {
    kind: u32,
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    rc_flags: u32,
    icq: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct MiscFrameRate {
    kind: u32,
    framerate: u32,
    flags: u32,
    reserved: [u32; 4],
}
