//! AAC-LC encode and playback.
//!
//! Encoding uses fdk-aac. Playback decodes to f32 and writes a PipeWire-free
//! ALSA device opened through libasound, so a missing sound card only logs.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deskbridge_proto::AudioPacket;
use fdk_aac::dec::{Decoder as AacDecoder, Transport as DecTransport};
use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};

use crate::error::PlatformError;

const AAC_FRAMES: usize = 1024;

pub struct AudioEncoder {
    encoder: Encoder,
    pending: Vec<f32>,
    cookie: Vec<u8>,
    rate: u32,
    channels: u8,
}

impl AudioEncoder {
    pub fn open(rate: u32, channels: u8) -> Result<Self, PlatformError> {
        if channels == 0 || channels > 2 || rate < 8_000 {
            return Err(PlatformError::new("audio format is not supported"));
        }
        let encoder = Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(96_000),
            sample_rate: rate,
            transport: Transport::Raw,
            channels: if channels == 1 { ChannelMode::Mono } else { ChannelMode::Stereo },
            audio_object_type: AudioObjectType::Mpeg4LowComplexity,
        })
        .map_err(|err| PlatformError::new(format!("AAC encoder did not start ({err})")))?;
        let info = encoder.info().map_err(|err| PlatformError::new(err))?;
        let cookie = if info.confSize > 0 {
            info.confBuf[..info.confSize as usize].to_vec()
        } else {
            aac_asc(rate, channels)
        };
        Ok(Self {
            encoder,
            pending: Vec::new(),
            cookie,
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

    /// Interleaved float samples. One raw AAC packet per 1024 frames.
    pub fn push(&mut self, interleaved_f32: &[f32]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(interleaved_f32);
        let block = AAC_FRAMES * usize::from(self.channels.max(1));
        let mut packets = Vec::new();
        while self.pending.len() >= block {
            let samples: Vec<f32> = self.pending.drain(..block).collect();
            if let Some(packet) = self.encode_block(&samples) {
                packets.push(packet);
            }
        }
        packets
    }

    fn encode_block(&self, samples: &[f32]) -> Option<Vec<u8>> {
        let pcm: Vec<i16> = samples.iter().copied().map(f32_to_i16).collect();
        let mut output = vec![0u8; 2048];
        let info = self.encoder.encode(&pcm, &mut output).ok()?;
        if info.output_size == 0 {
            return None;
        }
        output.truncate(info.output_size);
        Some(output)
    }

    pub fn packet(&self, payload: Vec<u8>, capture_us: u64) -> AudioPacket {
        let cookie = if self.cookie.is_empty() {
            aac_asc(self.rate, self.channels)
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

fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn i16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / 32768.0
}

/// AAC-LC AudioSpecificConfig. 48 kHz stereo is `[0x11, 0x90]`.
pub fn aac_asc(rate: u32, channels: u8) -> Vec<u8> {
    let freq = match rate {
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
    let channels = if channels == 1 { 1u16 } else { 2 };
    let bits = (2u16 << 11) | ((freq as u16) << 7) | (channels << 3);
    vec![(bits >> 8) as u8, (bits & 0xff) as u8]
}

pub struct AudioPlayer {
    tx: Option<Sender<AudioPacket>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || playback(rx));
        Self { tx: Some(tx), thread: Some(thread) }
    }

    pub fn push(&mut self, packet: &AudioPacket) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(packet.clone());
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn playback(rx: Receiver<AudioPacket>) {
    let mut decoder: Option<AacDecoder> = None;
    let mut cookie = Vec::new();
    let mut rate = 0u32;
    let mut channels = 0u8;
    let mut pcm: VecDeque<f32> = VecDeque::new();
    let mut device = AlsaOut::new();
    let mut started = false;
    while let Ok(packet) = rx.recv() {
        if packet.payload.is_empty() {
            continue;
        }
        let next_cookie = if packet.cookie.is_empty() {
            aac_asc(packet.sample_rate, packet.channels)
        } else {
            packet.cookie.clone()
        };
        if decoder.is_none() || cookie != next_cookie || rate != packet.sample_rate || channels != packet.channels {
            let mut opened = AacDecoder::new(DecTransport::Raw);
            if opened.config_raw(&next_cookie).is_err() {
                continue;
            }
            let _ = opened.set_min_output_channels(1);
            let _ = opened.set_max_output_channels(2);
            decoder = Some(opened);
            cookie = next_cookie;
            rate = packet.sample_rate;
            channels = packet.channels.max(1).min(2);
            pcm.clear();
            started = false;
            device.close();
        }
        let Some(decoder) = decoder.as_mut() else { continue };
        if decoder.fill(&packet.payload).is_err() {
            continue;
        }
        let mut decoded = vec![0i16; 4096];
        if decoder.decode_frame(&mut decoded).is_err() {
            continue;
        }
        let info = decoder.stream_info();
        let frames = info.frameSize.max(0) as usize;
        let decoded_channels = info.numChannels.max(1) as usize;
        let count = frames.saturating_mul(decoded_channels).min(decoded.len());
        for frame in 0..frames {
            let base = frame * decoded_channels;
            if base >= count {
                break;
            }
            let left = i16_to_f32(decoded[base]);
            let right = if decoded_channels > 1 && base + 1 < count {
                i16_to_f32(decoded[base + 1])
            } else {
                left
            };
            pcm.push_back(left);
            pcm.push_back(right);
        }
        let start_at = buffered_samples(rate, 40);
        let cap = buffered_samples(rate, 150);
        if pcm.len() > cap && cap > start_at {
            let drop_n = pcm.len() - start_at;
            pcm.drain(..drop_n);
        }
        if !started {
            if pcm.len() < start_at {
                continue;
            }
            started = true;
        }
        if !device.open(rate) {
            continue;
        }
        let mut block = Vec::with_capacity(pcm.len());
        block.extend(pcm.drain(..));
        device.write(&block);
    }
}

fn buffered_samples(rate: u32, ms: u32) -> usize {
    let rate = rate.max(8_000) as usize;
    rate * ms as usize / 1000 * 2
}

struct AlsaOut {
    lib: Option<libloading::Library>,
    pcm: *mut c_void,
    open: Option<unsafe extern "C" fn(*mut *mut c_void, *const i8, i32, i32) -> i32>,
    set_params: Option<unsafe extern "C" fn(*mut c_void, i32, i32, u32, u32, i32, u32) -> i32>,
    writei: Option<unsafe extern "C" fn(*mut c_void, *const c_void, u64) -> i64>,
    prepare: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    recover: Option<unsafe extern "C" fn(*mut c_void, i32, i32) -> i32>,
    close_pcm: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    logged: Arc<AtomicBool>,
    rate: u32,
}

unsafe impl Send for AlsaOut {}

impl AlsaOut {
    fn new() -> Self {
        Self {
            lib: None,
            pcm: std::ptr::null_mut(),
            open: None,
            set_params: None,
            writei: None,
            prepare: None,
            recover: None,
            close_pcm: None,
            logged: Arc::new(AtomicBool::new(false)),
            rate: 0,
        }
    }

    fn log_once(&self, message: &str) {
        if !self.logged.swap(true, Ordering::Relaxed) {
            eprintln!("deskbridge: {message}");
        }
    }

    fn ensure_lib(&mut self) -> bool {
        if self.lib.is_some() {
            return self.open.is_some();
        }
        let lib = match unsafe { libloading::Library::new("libasound.so.2") } {
            Ok(lib) => lib,
            Err(_) => {
                self.log_once("libasound.so.2 is not installed; audio playback is off");
                return false;
            }
        };
        unsafe fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Option<T> {
            let symbol: libloading::Symbol<T> = unsafe { lib.get(name).ok()? };
            Some(*symbol)
        }
        self.open = unsafe { sym(&lib, b"snd_pcm_open\0") };
        self.set_params = unsafe { sym(&lib, b"snd_pcm_set_params\0") };
        self.writei = unsafe { sym(&lib, b"snd_pcm_writei\0") };
        self.prepare = unsafe { sym(&lib, b"snd_pcm_prepare\0") };
        self.recover = unsafe { sym(&lib, b"snd_pcm_recover\0") };
        self.close_pcm = unsafe { sym(&lib, b"snd_pcm_close\0") };
        self.lib = Some(lib);
        self.open.is_some() && self.set_params.is_some() && self.writei.is_some()
    }

    fn open(&mut self, rate: u32) -> bool {
        if !self.pcm.is_null() && self.rate == rate {
            return true;
        }
        self.close();
        if !self.ensure_lib() {
            return false;
        }
        let Some(open) = self.open else { return false };
        let Some(set_params) = self.set_params else { return false };
        let name = c"default";
        let mut pcm = std::ptr::null_mut();
        let status = unsafe { open(&mut pcm, name.as_ptr(), 0, 0) };
        if status < 0 || pcm.is_null() {
            self.log_once("the default audio device did not open");
            return false;
        }
        // SND_PCM_FORMAT_FLOAT_LE = 14, SND_PCM_ACCESS_RW_INTERLEAVED = 3.
        let params = unsafe { set_params(pcm, 14, 3, 2, rate, 1, 40_000) };
        if params < 0 {
            self.log_once("the default audio device rejected float stereo");
            if let Some(close) = self.close_pcm {
                unsafe { close(pcm) };
            }
            return false;
        }
        self.pcm = pcm;
        self.rate = rate;
        true
    }

    fn write(&mut self, interleaved: &[f32]) {
        if self.pcm.is_null() || interleaved.len() < 2 {
            return;
        }
        let Some(writei) = self.writei else { return };
        let frames = (interleaved.len() / 2) as u64;
        let wrote = unsafe { writei(self.pcm, interleaved.as_ptr().cast(), frames) };
        if wrote < 0 {
            if let Some(recover) = self.recover {
                unsafe { recover(self.pcm, wrote as i32, 1) };
            } else if let Some(prepare) = self.prepare {
                unsafe { prepare(self.pcm) };
            }
        }
    }

    fn close(&mut self) {
        if self.pcm.is_null() {
            return;
        }
        if let Some(close) = self.close_pcm {
            unsafe { close(self.pcm) };
        }
        self.pcm = std::ptr::null_mut();
        self.rate = 0;
    }
}

impl Drop for AlsaOut {
    fn drop(&mut self) {
        self.close();
    }
}

#[allow(dead_code)]
fn _keep_duration(duration: Duration) -> Duration {
    duration
}
