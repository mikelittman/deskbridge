//! System audio: AAC-LC through AudioToolbox, played with a short queue.
//!
//! ScreenCaptureKit hands us PCM. Each 1024-frame block becomes one AAC packet
//! so a slow link still has room for the picture. Playback keeps about 40 ms
//! and drops anything older than 150 ms, so sound stays with the desktop.

use std::collections::VecDeque;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use deskbridge_proto::AudioPacket;

use crate::error::PlatformError;

const AAC_FRAMES: usize = 1024;
const QUEUE_FRAMES: u32 = 512;

const FORMAT_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_AAC: u32 = u32::from_be_bytes(*b"aac ");
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_PACKED: u32 = 1 << 3;
const PROP_BITRATE: u32 = u32::from_be_bytes(*b"brat");
const PROP_ENCODE_COOKIE: u32 = u32::from_be_bytes(*b"cmgc");
const PROP_DECODE_COOKIE: u32 = u32::from_be_bytes(*b"dmgc");

#[repr(C)]
struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct PacketDescription {
    start_offset: i64,
    variable_frames: u32,
    data_byte_size: u32,
}

#[repr(C)]
struct QueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut PacketDescription,
    packet_description_count: u32,
}

type Converter = *mut c_void;
type Queue = *mut c_void;
type QueueBufferRef = *mut QueueBuffer;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioConverterNew(src: *const StreamFormat, dst: *const StreamFormat, out: *mut Converter) -> i32;
    fn AudioConverterDispose(converter: Converter) -> i32;
    fn AudioConverterSetProperty(converter: Converter, property: u32, size: u32, data: *const c_void) -> i32;
    fn AudioConverterGetPropertyInfo(
        converter: Converter,
        property: u32,
        size: *mut u32,
        writable: *mut u8,
    ) -> i32;
    fn AudioConverterGetProperty(converter: Converter, property: u32, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: Converter,
        input: extern "C" fn(
            Converter,
            *mut u32,
            *mut AudioBufferList,
            *mut *mut PacketDescription,
            *mut c_void,
        ) -> i32,
        user: *mut c_void,
        packets: *mut u32,
        output: *mut AudioBufferList,
        description: *mut PacketDescription,
    ) -> i32;

    fn AudioQueueNewOutput(
        format: *const StreamFormat,
        callback: extern "C" fn(*mut c_void, Queue, QueueBufferRef),
        user: *mut c_void,
        run_loop: *mut c_void,
        mode: *const c_void,
        flags: u32,
        queue: *mut Queue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: Queue, bytes: u32, buffer: *mut QueueBufferRef) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: Queue,
        buffer: QueueBufferRef,
        descriptions: u32,
        packets: *const PacketDescription,
    ) -> i32;
    fn AudioQueueStart(queue: Queue, start_time: *const c_void) -> i32;
    fn AudioQueueStop(queue: Queue, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: Queue, immediate: u8) -> i32;
}

fn pcm_format(rate: u32, channels: u8) -> StreamFormat {
    let channels = u32::from(channels);
    StreamFormat {
        sample_rate: f64::from(rate),
        format_id: FORMAT_PCM,
        format_flags: FLAG_FLOAT | FLAG_PACKED,
        bytes_per_packet: 4 * channels,
        frames_per_packet: 1,
        bytes_per_frame: 4 * channels,
        channels_per_frame: channels,
        bits_per_channel: 32,
        reserved: 0,
    }
}

fn aac_format(rate: u32, channels: u8) -> StreamFormat {
    StreamFormat {
        sample_rate: f64::from(rate),
        format_id: FORMAT_AAC,
        format_flags: 0,
        bytes_per_packet: 0,
        frames_per_packet: AAC_FRAMES as u32,
        bytes_per_frame: 0,
        channels_per_frame: u32::from(channels),
        bits_per_channel: 0,
        reserved: 0,
    }
}

fn asc(rate: u32, channels: u8) -> Vec<u8> {
    let index = match rate {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => 3,
    };
    let channels = channels.min(2);
    let first = (2 << 3) | ((index & 0x0E) >> 1);
    let second = ((index & 0x01) << 7) | (channels << 3);
    vec![first, second]
}

struct PcmInput {
    ptr: *const f32,
    frames_left: u32,
    channels: u32,
}

extern "C" fn pull_pcm(
    _: Converter,
    packets: *mut u32,
    data: *mut AudioBufferList,
    descriptions: *mut *mut PacketDescription,
    user: *mut c_void,
) -> i32 {
    let input = unsafe { &mut *(user as *mut PcmInput) };
    if !descriptions.is_null() {
        unsafe { *descriptions = std::ptr::null_mut() };
    }
    let want = unsafe { *packets };
    let give = want.min(input.frames_left);
    unsafe { *packets = give };
    if give == 0 {
        return 0;
    }
    unsafe {
        let list = &mut *data;
        list.number_buffers = 1;
        list.buffers[0].number_channels = input.channels;
        list.buffers[0].data_byte_size = give * input.channels * 4;
        list.buffers[0].data = input.ptr as *mut c_void;
        input.ptr = input.ptr.add((give * input.channels) as usize);
        input.frames_left -= give;
    }
    0
}

struct AacInput {
    data: *const u8,
    size: u32,
    offered: bool,
    description: PacketDescription,
}

extern "C" fn pull_aac(
    _: Converter,
    packets: *mut u32,
    data: *mut AudioBufferList,
    descriptions: *mut *mut PacketDescription,
    user: *mut c_void,
) -> i32 {
    let input = unsafe { &mut *(user as *mut AacInput) };
    if input.offered || input.size == 0 {
        unsafe { *packets = 0 };
        return 0;
    }
    input.offered = true;
    input.description = PacketDescription {
        start_offset: 0,
        variable_frames: 0,
        data_byte_size: input.size,
    };
    unsafe {
        *packets = 1;
        if !descriptions.is_null() {
            *descriptions = &mut input.description;
        }
        let list = &mut *data;
        list.number_buffers = 1;
        list.buffers[0].number_channels = 0;
        list.buffers[0].data_byte_size = input.size;
        list.buffers[0].data = input.data as *mut c_void;
    }
    0
}

pub struct AudioEncoder {
    converter: Converter,
    pending: Vec<f32>,
    cookie: Vec<u8>,
    rate: u32,
    channels: u8,
}

impl AudioEncoder {
    pub fn open(rate: u32, channels: u8, bitrate: u32) -> Result<Self, PlatformError> {
        if channels == 0 || channels > 2 || rate < 8_000 {
            return Err(PlatformError::new("audio format is not supported"));
        }
        let src = pcm_format(rate, channels);
        let dst = aac_format(rate, channels);
        let mut converter: Converter = std::ptr::null_mut();
        let status = unsafe { AudioConverterNew(&src, &dst, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(PlatformError::new(format!("AAC encoder did not start ({status})")));
        }
        let bitrate = bitrate.max(32_000);
        let set = unsafe {
            AudioConverterSetProperty(converter, PROP_BITRATE, 4, std::ptr::from_ref(&bitrate).cast())
        };
        if set != 0 {
            unsafe { AudioConverterDispose(converter) };
            return Err(PlatformError::new(format!("AAC bitrate was rejected ({set})")));
        }
        Ok(Self {
            converter,
            pending: Vec::new(),
            cookie: Vec::new(),
            rate,
            channels,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Interleaved float samples. Returns one AAC packet per full frame.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(interleaved);
        let block = AAC_FRAMES * self.channels as usize;
        let mut packets = Vec::new();
        while self.pending.len() >= block {
            let samples: Vec<f32> = self.pending.drain(..block).collect();
            if let Some(packet) = self.encode_block(&samples) {
                packets.push(packet);
            }
        }
        packets
    }

    fn encode_block(&mut self, samples: &[f32]) -> Option<Vec<u8>> {
        let mut input = PcmInput {
            ptr: samples.as_ptr(),
            frames_left: AAC_FRAMES as u32,
            channels: u32::from(self.channels),
        };
        let mut encoded = vec![0u8; 4096];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: u32::from(self.channels),
                data_byte_size: encoded.len() as u32,
                data: encoded.as_mut_ptr().cast(),
            }],
        };
        let mut produced = 1u32;
        let mut description = PacketDescription {
            start_offset: 0,
            variable_frames: 0,
            data_byte_size: 0,
        };
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                pull_pcm,
                std::ptr::from_mut(&mut input).cast(),
                &mut produced,
                &mut list,
                &mut description,
            )
        };
        if status != 0 || produced == 0 {
            return None;
        }
        self.refresh_cookie();
        let size = if description.data_byte_size > 0 {
            description.data_byte_size as usize
        } else {
            list.buffers[0].data_byte_size as usize
        };
        if size == 0 || size > encoded.len() {
            return None;
        }
        encoded.truncate(size);
        Some(encoded)
    }

    fn refresh_cookie(&mut self) {
        if !self.cookie.is_empty() {
            return;
        }
        let mut size = 0u32;
        let info = unsafe {
            AudioConverterGetPropertyInfo(self.converter, PROP_ENCODE_COOKIE, &mut size, std::ptr::null_mut())
        };
        if info != 0 || size == 0 || size > 64 {
            self.cookie = asc(self.rate, self.channels);
            return;
        }
        let mut cookie = vec![0u8; size as usize];
        let got = unsafe {
            AudioConverterGetProperty(
                self.converter,
                PROP_ENCODE_COOKIE,
                &mut size,
                cookie.as_mut_ptr().cast(),
            )
        };
        if got != 0 {
            self.cookie = asc(self.rate, self.channels);
            return;
        }
        cookie.truncate(size as usize);
        self.cookie = cookie;
    }

    pub fn packet(&self, payload: Vec<u8>, capture_us: u64) -> AudioPacket {
        let cookie = if self.cookie.is_empty() {
            asc(self.rate, self.channels)
        } else {
            self.cookie.clone()
        };
        AudioPacket {
            sample_rate: self.rate,
            channels: self.channels,
            capture_us,
            cookie,
            payload,
        }
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            unsafe { AudioConverterDispose(self.converter) };
        }
    }
}

pub struct AudioDecoder {
    converter: Converter,
    channels: u8,
    logged: bool,
}

impl AudioDecoder {
    pub fn open(rate: u32, channels: u8, cookie: &[u8]) -> Result<Self, PlatformError> {
        let src = aac_format(rate, channels);
        let dst = pcm_format(rate, channels);
        let mut converter: Converter = std::ptr::null_mut();
        let status = unsafe { AudioConverterNew(&src, &dst, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(PlatformError::new(format!("AAC decoder did not start ({status})")));
        }
        let cookie = if cookie.is_empty() { asc(rate, channels) } else { cookie.to_vec() };
        let set = unsafe {
            AudioConverterSetProperty(
                converter,
                PROP_DECODE_COOKIE,
                cookie.len() as u32,
                cookie.as_ptr().cast(),
            )
        };
        if set != 0 {
            unsafe { AudioConverterDispose(converter) };
            return Err(PlatformError::new(format!("AAC decoder rejected its setup ({set})")));
        }
        Ok(Self { converter, channels, logged: false })
    }

    pub fn push(&mut self, packet: &[u8]) -> Vec<f32> {
        if packet.is_empty() {
            return Vec::new();
        }
        let mut input = AacInput {
            data: packet.as_ptr(),
            size: packet.len() as u32,
            offered: false,
            description: PacketDescription {
                start_offset: 0,
                variable_frames: 0,
                data_byte_size: 0,
            },
        };
        let samples = AAC_FRAMES * self.channels as usize;
        let mut pcm = vec![0f32; samples];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: u32::from(self.channels),
                data_byte_size: (pcm.len() * 4) as u32,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        let mut produced = AAC_FRAMES as u32;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                pull_aac,
                std::ptr::from_mut(&mut input).cast(),
                &mut produced,
                &mut list,
                std::ptr::null_mut(),
            )
        };
        if status != 0 || produced == 0 {
            if !self.logged {
                eprintln!("audio decode ({status})");
                self.logged = true;
            }
            return Vec::new();
        }
        let bytes = list.buffers[0].data_byte_size as usize;
        let count = (bytes / 4).min(pcm.len());
        pcm.truncate(count);
        pcm
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            unsafe { AudioConverterDispose(self.converter) };
        }
    }
}

struct Ring {
    samples: VecDeque<f32>,
    channels: usize,
    max_samples: usize,
    keep_samples: usize,
}

struct PlayerInner {
    ring: Mutex<Ring>,
}

extern "C" fn render(user: *mut c_void, queue: Queue, buffer: QueueBufferRef) {
    if user.is_null() || buffer.is_null() {
        return;
    }
    let inner = unsafe { &*(user as *const PlayerInner) };
    let buf = unsafe { &mut *buffer };
    let bytes = buf.audio_data_bytes_capacity as usize;
    let slots = bytes / 4;
    let dst = unsafe { std::slice::from_raw_parts_mut(buf.audio_data as *mut f32, slots) };
    dst.fill(0.0);
    if let Ok(mut ring) = inner.ring.lock() {
        if ring.samples.len() > ring.max_samples {
            let extra = ring.samples.len() - ring.keep_samples;
            let extra = extra / ring.channels * ring.channels;
            ring.samples.drain(..extra);
        }
        let n = slots.min(ring.samples.len());
        for (index, sample) in ring.samples.drain(..n).enumerate() {
            dst[index] = sample;
        }
    }
    buf.audio_data_byte_size = bytes as u32;
    unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
}

struct Playing {
    inner: Box<PlayerInner>,
    queue: Queue,
    decoder: AudioDecoder,
    rate: u32,
    channels: u8,
    started: AtomicBool,
}

impl Playing {
    fn open(packet: &AudioPacket) -> Result<Self, PlatformError> {
        let decoder = AudioDecoder::open(packet.sample_rate, packet.channels, &packet.cookie)?;
        let channels = packet.channels as usize;
        let keep = packet.sample_rate as usize * channels * 40 / 1000;
        let max = packet.sample_rate as usize * channels * 150 / 1000;
        let inner = Box::new(PlayerInner {
            ring: Mutex::new(Ring {
                samples: VecDeque::new(),
                channels,
                max_samples: max.max(channels),
                keep_samples: keep.max(channels),
            }),
        });
        let format = pcm_format(packet.sample_rate, packet.channels);
        let mut queue: Queue = std::ptr::null_mut();
        let user = &*inner as *const PlayerInner as *mut c_void;
        let status = unsafe {
            AudioQueueNewOutput(&format, render, user, std::ptr::null_mut(), std::ptr::null(), 0, &mut queue)
        };
        if status != 0 || queue.is_null() {
            return Err(PlatformError::new(format!("audio output did not start ({status})")));
        }
        let bytes = QUEUE_FRAMES * packet.channels as u32 * 4;
        for _ in 0..3 {
            let mut buffer: QueueBufferRef = std::ptr::null_mut();
            let allocated = unsafe { AudioQueueAllocateBuffer(queue, bytes, &mut buffer) };
            if allocated != 0 || buffer.is_null() {
                unsafe { AudioQueueDispose(queue, 1) };
                return Err(PlatformError::new(format!("audio buffer was rejected ({allocated})")));
            }
            render(user, queue, buffer);
        }
        Ok(Self {
            inner,
            queue,
            decoder,
            rate: packet.sample_rate,
            channels: packet.channels,
            started: AtomicBool::new(false),
        })
    }

    fn push_packet(&mut self, payload: &[u8]) {
        let pcm = self.decoder.push(payload);
        if pcm.is_empty() {
            return;
        }
        let start = {
            let mut ring = self.inner.ring.lock().expect("audio ring");
            ring.samples.extend(pcm);
            !self.started.load(Ordering::Relaxed) && ring.samples.len() >= ring.keep_samples
        };
        if start {
            let status = unsafe { AudioQueueStart(self.queue, std::ptr::null()) };
            if status == 0 {
                self.started.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        if !self.queue.is_null() {
            unsafe {
                AudioQueueStop(self.queue, 1);
                AudioQueueDispose(self.queue, 1);
            }
        }
    }
}

/// Plays AAC packets from the host. The queue starts once a few frames are buffered.
pub struct AudioPlayer {
    playing: Option<Playing>,
    failed: bool,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self { playing: None, failed: false }
    }

    pub fn push(&mut self, packet: &AudioPacket) {
        if self.failed || packet.payload.is_empty() {
            return;
        }
        let same = self
            .playing
            .as_ref()
            .is_some_and(|playing| playing.channels == packet.channels && playing.rate == packet.sample_rate);
        if !same {
            match Playing::open(packet) {
                Ok(playing) => self.playing = Some(playing),
                Err(err) => {
                    eprintln!("audio playback: {err}");
                    self.failed = true;
                    self.playing = None;
                    return;
                }
            }
        }
        if let Some(playing) = self.playing.as_mut() {
            playing.push_packet(&packet.payload);
        }
    }
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_survives_aac() {
        let mut encoder = AudioEncoder::open(48_000, 2, 96_000).expect("encoder");
        let mut pcm = Vec::with_capacity(48_000 * 2);
        for frame in 0..48_000 {
            let sample = (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.4;
            pcm.push(sample);
            pcm.push(sample);
        }
        let packets = encoder.push(&pcm);
        assert!(packets.len() >= 40, "packets {}", packets.len());
        let cookie = encoder.packet(Vec::new(), 0).cookie;
        assert!(!cookie.is_empty());
        let mut decoder = AudioDecoder::open(48_000, 2, &cookie).expect("decoder");
        let mut decoded = Vec::new();
        for packet in &packets {
            decoded.extend(decoder.push(packet));
        }
        assert!(decoded.len() > 8_000, "decoded {}", decoded.len());
        let energy = decoded.iter().map(|sample| sample * sample).sum::<f32>() / decoded.len() as f32;
        assert!(energy > 0.01, "energy {energy}");
    }
}
