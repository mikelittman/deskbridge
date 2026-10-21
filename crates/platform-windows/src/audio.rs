//! AAC-LC through Media Foundation, played with WASAPI in shared mode.
//!
//! Playback waits until about 40 ms is buffered and drops to that when more
//! than about 150 ms has piled up. A missing device is reported once.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deskbridge_proto::AudioPacket;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::MediaFoundation::{
    IMFTransform, MFAudioFormat_AAC, MFAudioFormat_Float, MFAudioFormat_PCM,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_E_TRANSFORM_NEED_MORE_INPUT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::codec::{copy_sample_bytes, make_audio_type, make_sample, open_aac_transform, take_output_sample, MfGuard};
use crate::error::PlatformError;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

pub struct AudioEncoder {
    transform: IMFTransform,
    _mf: MfGuard,
    pending: Vec<i16>,
    frame_samples: usize,
    cookie: Vec<u8>,
    rate: u32,
    channels: u8,
    provides_samples: bool,
    output_bytes: u32,
}

impl AudioEncoder {
    pub fn open(rate: u32, channels: u8) -> Result<Self, PlatformError> {
        if channels == 0 || channels > 2 || !(8_000..=96_000).contains(&rate) {
            return Err(PlatformError::new("audio format is not supported"));
        }
        let mf = MfGuard::startup()?;
        let transform = open_aac_transform(true)?;
        let cookie = audio_specific_config(rate, channels);
        unsafe {
            let output = make_audio_type(MFAudioFormat_AAC, rate, channels, 16, 12_000, Some(&cookie))?;
            transform.SetOutputType(0, &output, 0).map_err(PlatformError::new)?;
            let input = make_audio_type(
                MFAudioFormat_PCM,
                rate,
                channels,
                16,
                rate * u32::from(channels) * 2,
                None,
            )?;
            transform.SetInputType(0, &input, 0).map_err(PlatformError::new)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ok();
        }
        let (provides_samples, output_bytes) = output_buffer(&transform);
        Ok(Self {
            transform,
            _mf: mf,
            pending: Vec::new(),
            frame_samples: 1024 * usize::from(channels),
            cookie,
            rate,
            channels,
            provides_samples,
            output_bytes,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn push(&mut self, interleaved_f32: &[f32]) -> Vec<Vec<u8>> {
        self.pending.extend(interleaved_f32.iter().map(|sample| {
            (sample.clamp(-1.0, 1.0) * 32767.0) as i16
        }));
        let mut packets = Vec::new();
        while self.pending.len() >= self.frame_samples {
            let block: Vec<i16> = self.pending.drain(..self.frame_samples).collect();
            if let Some(packet) = self.encode_block(&block) {
                packets.push(packet);
            }
        }
        packets
    }

    pub fn packet(&self, payload: Vec<u8>, capture_us: u64) -> AudioPacket {
        let cookie = if self.cookie.is_empty() {
            audio_specific_config(self.rate, self.channels)
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

fn audio_specific_config(rate: u32, channels: u8) -> Vec<u8> {
    if rate == 48_000 && channels == 2 {
        return vec![0x11, 0x90];
    }
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
    let first = (2 << 3) | ((index & 0x0e) >> 1);
    let second = ((index & 0x01) << 7) | (channels << 3);
    vec![first, second]
}

impl AudioEncoder {
    fn encode_block(&mut self, pcm: &[i16]) -> Option<Vec<u8>> {
        let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        let sample = make_sample(&bytes).ok()?;
        unsafe { self.transform.ProcessInput(0, &sample, 0).ok()? };
        pull_transform(&self.transform, self.provides_samples, self.output_bytes)
    }
}

struct PcmDecoder {
    transform: IMFTransform,
    _mf: MfGuard,
    provides_samples: bool,
    output_bytes: u32,
    float_out: bool,
}

impl PcmDecoder {
    fn open(rate: u32, channels: u8, cookie: &[u8]) -> Result<Self, PlatformError> {
        let mf = MfGuard::startup()?;
        let cookie = if cookie.is_empty() {
            audio_specific_config(rate, channels)
        } else {
            cookie.to_vec()
        };
        let transform = open_aac_transform(false)?;
        unsafe {
            let input = make_audio_type(MFAudioFormat_AAC, rate, channels, 16, 12_000, Some(&cookie))?;
            transform.SetInputType(0, &input, 0).map_err(PlatformError::new)?;
            let output = make_audio_type(
                MFAudioFormat_PCM,
                rate,
                channels,
                16,
                rate * u32::from(channels) * 2,
                None,
            )?;
            if transform.SetOutputType(0, &output, 0).is_err() {
                let available = transform
                    .GetOutputAvailableType(0, 0)
                    .map_err(PlatformError::new)?;
                transform.SetOutputType(0, &available, 0).map_err(PlatformError::new)?;
            }
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ok();
        }
        let float_out = unsafe { transform.GetOutputCurrentType(0) }
            .ok()
            .and_then(|media| unsafe { media.GetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE) }.ok())
            == Some(MFAudioFormat_Float);
        let (provides_samples, output_bytes) = output_buffer(&transform);
        Ok(Self {
            transform,
            _mf: mf,
            provides_samples,
            output_bytes,
            float_out,
        })
    }

    fn push(&mut self, packet: &[u8]) -> Vec<f32> {
        if packet.is_empty() {
            return Vec::new();
        }
        let Ok(sample) = make_sample(packet) else {
            return Vec::new();
        };
        if unsafe { self.transform.ProcessInput(0, &sample, 0) }.is_err() {
            return Vec::new();
        }
        let mut pcm = Vec::new();
        for _ in 0..4 {
            let Some(bytes) = pull_transform(&self.transform, self.provides_samples, self.output_bytes) else {
                break;
            };
            if self.float_out {
                pcm.extend(bytes.chunks_exact(4).map(|word| {
                    f32::from_le_bytes([word[0], word[1], word[2], word[3]])
                }));
            } else {
                pcm.extend(
                    bytes
                        .chunks_exact(2)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0),
                );
            }
        }
        pcm
    }
}

fn output_buffer(transform: &IMFTransform) -> (bool, u32) {
    let info = unsafe { transform.GetOutputStreamInfo(0) }.unwrap_or_default();
    let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
    let bytes = if info.cbSize == 0 { 16 * 1024 } else { info.cbSize.max(2048) };
    (provides, bytes)
}

fn pull_transform(transform: &IMFTransform, provides: bool, bytes: u32) -> Option<Vec<u8>> {
    let mut buffer = MFT_OUTPUT_DATA_BUFFER::default();
    if !provides {
        buffer.pSample = ManuallyDrop::new(Some(make_sample(&vec![0u8; bytes.max(1) as usize]).ok()?));
    }
    let mut status = 0u32;
    let result = unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
    let sample = take_output_sample(&mut buffer.pSample);
    let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };
    match result {
        Ok(()) => sample.as_ref().and_then(|sample| copy_sample_bytes(sample).ok()).filter(|out| !out.is_empty()),
        Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => None,
        Err(_) => None,
    }
}

struct Ring {
    samples: VecDeque<f32>,
    channels: usize,
    rate: u32,
    max_samples: usize,
    keep_samples: usize,
}

struct PlayerShared {
    ring: Mutex<Ring>,
    stop: AtomicBool,
    started: AtomicBool,
}

struct Playing {
    shared: Arc<PlayerShared>,
    thread: Option<JoinHandle<()>>,
    decoder: PcmDecoder,
    rate: u32,
    channels: u8,
    cookie: Vec<u8>,
}

impl Playing {
    fn open(packet: &AudioPacket) -> Result<Self, PlatformError> {
        let _mf = MfGuard::startup().ok();
        let decoder = PcmDecoder::open(packet.sample_rate, packet.channels, &packet.cookie)?;
        let device_format = match output_format() {
            Ok(format) => format,
            Err(err) => return Err(err),
        };
        let channels = device_format.channels.max(1) as usize;
        let rate = device_format.rate.max(1);
        let keep = rate as usize * channels * 40 / 1000;
        let max = rate as usize * channels * 150 / 1000;
        let shared = Arc::new(PlayerShared {
            ring: Mutex::new(Ring {
                samples: VecDeque::new(),
                channels,
                rate,
                max_samples: max.max(channels),
                keep_samples: keep.max(channels),
            }),
            stop: AtomicBool::new(false),
            started: AtomicBool::new(false),
        });
        let shared_thread = Arc::clone(&shared);
        let thread = thread::spawn(move || render_loop(shared_thread, device_format));
        Ok(Self {
            shared,
            thread: Some(thread),
            decoder,
            rate: packet.sample_rate,
            channels: packet.channels,
            cookie: packet.cookie.clone(),
        })
    }

    fn push_packet(&mut self, payload: &[u8]) {
        let pcm = self.decoder.push(payload);
        if pcm.is_empty() {
            return;
        }
        let mut ring = self.shared.ring.lock().expect("audio ring");
        let converted = resample(
            &pcm,
            self.rate,
            usize::from(self.channels.max(1)),
            ring.rate,
            ring.channels,
        );
        ring.samples.extend(converted);
        if ring.samples.len() > ring.max_samples {
            let extra = ring.samples.len() - ring.keep_samples;
            let extra = extra / ring.channels * ring.channels;
            ring.samples.drain(..extra);
        }
        if !self.shared.started.load(Ordering::Relaxed) && ring.samples.len() >= ring.keep_samples {
            self.shared.started.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct DeviceFormat {
    rate: u32,
    channels: u8,
    bits: u16,
    is_float: bool,
    block: u16,
    mix: Vec<u8>,
}

fn output_format() -> Result<DeviceFormat, PlatformError> {
    let _ = MfGuard::startup()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(PlatformError::new)?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(PlatformError::new)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(PlatformError::new)?;
        let mix = client.GetMixFormat().map_err(PlatformError::new)?;
        let format = describe_mix(mix);
        CoTaskMemFree(Some(mix.cast()));
        Ok(format)
    }
}

fn describe_mix(mix: *mut WAVEFORMATEX) -> DeviceFormat {
    let format = unsafe { std::ptr::read_unaligned(mix) };
    let mut is_float = format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT;
    if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE && format.cbSize >= 22 {
        let ext = unsafe { std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE) };
        is_float = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.SubFormat)) } == MFAudioFormat_Float;
    }
    let bytes = std::mem::size_of::<WAVEFORMATEX>() + format.cbSize as usize;
    let mix_bytes = unsafe { std::slice::from_raw_parts(mix.cast::<u8>(), bytes).to_vec() };
    DeviceFormat {
        rate: format.nSamplesPerSec,
        channels: format.nChannels.max(1) as u8,
        bits: format.wBitsPerSample,
        is_float,
        block: format.nBlockAlign.max(1),
        mix: mix_bytes,
    }
}

fn render_loop(shared: Arc<PlayerShared>, format: DeviceFormat) {
    let _mf = MfGuard::startup();
    let (client, render, event) = match unsafe { open_render_client(&format) } {
        Ok(opened) => opened,
        Err(err) => {
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                eprintln!("audio playback: {err}");
            }
            return;
        }
    };
    while !shared.stop.load(Ordering::Relaxed) {
        if !shared.started.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if unsafe { client.Start() }.is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    while !shared.stop.load(Ordering::Relaxed) {
        let waited = unsafe { WaitForSingleObject(event, 50) };
        if waited != WAIT_OBJECT_0 && shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let padding = unsafe { client.GetCurrentPadding() }.unwrap_or(0);
        let size = unsafe { client.GetBufferSize() }.unwrap_or(0);
        let frames = size.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        let Ok(dest) = (unsafe { render.GetBuffer(frames) }) else {
            continue;
        };
        if dest.is_null() {
            let _ = unsafe { render.ReleaseBuffer(frames, 2) };
            continue;
        }
        fill_render(&shared, &format, dest, frames);
        let _ = unsafe { render.ReleaseBuffer(frames, 0) };
    }
    unsafe {
        client.Stop().ok();
        CloseHandle(event).ok();
    }
}

unsafe fn open_render_client(format: &DeviceFormat) -> Result<(IAudioClient, IAudioRenderClient, HANDLE), PlatformError> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(PlatformError::new)?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(PlatformError::new)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(PlatformError::new)?;
    let event = CreateEventW(None, BOOL(0), BOOL(0), PCWSTR::null()).map_err(PlatformError::new)?;
    let mix = format.mix.as_ptr() as *const WAVEFORMATEX;
    client
        .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None)
        .map_err(PlatformError::new)?;
    client.SetEventHandle(event).map_err(PlatformError::new)?;
    let render = client.GetService::<IAudioRenderClient>().map_err(PlatformError::new)?;
    Ok((client, render, event))
}

fn fill_render(shared: &PlayerShared, format: &DeviceFormat, dest: *mut u8, frames: u32) {
    let channels = usize::from(format.channels.max(1));
    let samples = frames as usize * channels;
    let mut taken = Vec::with_capacity(samples);
    if let Ok(mut ring) = shared.ring.lock() {
        if ring.samples.len() > ring.max_samples {
            let extra = ring.samples.len() - ring.keep_samples;
            let extra = extra / ring.channels.max(1) * ring.channels.max(1);
            ring.samples.drain(..extra);
        }
        let n = samples.min(ring.samples.len());
        taken.extend(ring.samples.drain(..n));
    }
    unsafe {
        let bytes = frames as usize * format.block as usize;
        std::ptr::write_bytes(dest, 0, bytes);
        if format.is_float && format.bits == 32 {
            let dst = std::slice::from_raw_parts_mut(dest as *mut f32, samples);
            for (index, sample) in taken.iter().enumerate() {
                dst[index] = *sample;
            }
        } else if format.bits == 16 {
            let dst = std::slice::from_raw_parts_mut(dest as *mut i16, samples);
            for (index, sample) in taken.iter().enumerate() {
                dst[index] = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
        }
    }
}

fn resample(input: &[f32], in_rate: u32, in_channels: usize, out_rate: u32, out_channels: usize) -> Vec<f32> {
    if input.is_empty() || in_channels == 0 || out_channels == 0 || in_rate == 0 || out_rate == 0 {
        return Vec::new();
    }
    let in_frames = input.len() / in_channels;
    if in_frames == 0 {
        return Vec::new();
    }
    let out_frames = ((in_frames as u64 * u64::from(out_rate)) / u64::from(in_rate)).max(1) as usize;
    let mut out = Vec::with_capacity(out_frames * out_channels);
    for frame in 0..out_frames {
        let src = (frame as u64 * u64::from(in_rate)) / u64::from(out_rate);
        let src = (src as usize).min(in_frames - 1);
        for channel in 0..out_channels {
            let from = channel.min(in_channels - 1);
            out.push(input[src * in_channels + from]);
        }
    }
    out
}

/// Plays AAC packets from the host. The device starts once about 40 ms is buffered.
pub struct AudioPlayer {
    playing: Option<Playing>,
    failed: bool,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self {
            playing: None,
            failed: false,
        }
    }

    pub fn push(&mut self, packet: &AudioPacket) {
        if self.failed || packet.payload.is_empty() {
            return;
        }
        let same = self.playing.as_ref().is_some_and(|playing| {
            playing.channels == packet.channels && playing.rate == packet.sample_rate && playing.cookie == packet.cookie
        });
        if !same {
            match Playing::open(packet) {
                Ok(playing) => self.playing = Some(playing),
                Err(err) => {
                    static ONCE: AtomicBool = AtomicBool::new(false);
                    if !ONCE.swap(true, Ordering::Relaxed) {
                        eprintln!("audio playback: {err}");
                    }
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
