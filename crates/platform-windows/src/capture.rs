//! Windows.Graphics.Capture for the picture, WASAPI loopback for system audio.
//!
//! The newest frame replaces the previous one. Audio keeps about a second and drops older samples.

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deskbridge_proto::DisplayInfo;
use deskbridge_session::mono_us;
use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::MediaFoundation::MFAudioFormat_Float;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::codec::{fit_inside, sample_hash, scale_bgra, MfGuard};
use crate::error::PlatformError;

const IID_CAPTURE_INTEROP: GUID = GUID::from_u128(0x3628e81b_3cac_4c60_b7f4_23ce0e0c3356);
const IID_DXGI_ACCESS: GUID = GUID::from_u128(0xa9b3d012_3df2_4ee3_b8d1_8695f457d3c1);
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[link(name = "d3d11")]
extern "system" {
    fn CreateDirect3D11DeviceFromDXGIDevice(dxgi: *mut core::ffi::c_void, device: *mut *mut core::ffi::c_void) -> HRESULT;
}

#[repr(C)]
struct UnknownVtbl {
    query: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
}

#[repr(C)]
struct CaptureInteropVtbl {
    base: UnknownVtbl,
    create_for_window: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        windows::Win32::Foundation::HWND,
        *const GUID,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    create_for_monitor:
        unsafe extern "system" fn(*mut core::ffi::c_void, HMONITOR, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
}

#[repr(C)]
struct DxgiAccessVtbl {
    base: UnknownVtbl,
    get_interface: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
}

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
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub capture_us: u64,
    /// `Some(0.0)` when a pixel sample matches the previous frame, otherwise `Some(1.0)`.
    pub changed: Option<f32>,
}

pub struct PcmChunk {
    pub rate: u32,
    pub channels: u8,
    pub interleaved: Vec<f32>,
}

struct Latest {
    frame: Mutex<Option<CapturedFrame>>,
    cv: Condvar,
}

struct AudioState {
    pcm: VecDeque<f32>,
    rate: u32,
    channels: u8,
    logged: bool,
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    latest: Arc<Latest>,
    audio: Arc<Mutex<AudioState>>,
    frame_thread: Mutex<Option<JoinHandle<()>>>,
    audio_thread: Mutex<Option<JoinHandle<()>>>,
    pub info: DisplayInfo,
    pub geometry: DisplayGeometry,
    pub fps: u32,
}

impl Capture {
    pub fn start(display_index: usize, limit: Option<(u32, u32)>) -> Result<Self, PlatformError> {
        let monitors = monitors()?;
        let monitor = monitors.get(display_index).copied().ok_or_else(|| {
            PlatformError::new(format!(
                "display {display_index} is not connected ({count} available)",
                count = monitors.len()
            ))
        })?;
        let placed = monitor_place(monitor)?;
        let native_w = (placed.right - placed.left).max(2) as u32;
        let native_h = (placed.bottom - placed.top).max(2) as u32;
        let (pixel_width, pixel_height) = match limit {
            Some((max_w, max_h)) => fit_inside(native_w, native_h, max_w, max_h),
            None => fit_inside(native_w, native_h, native_w, native_h),
        };
        let refresh = refresh_millihz(monitor);
        let fps = if refresh == 0 {
            60
        } else {
            (refresh / 1000).clamp(30, 120)
        };
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Latest {
            frame: Mutex::new(None),
            cv: Condvar::new(),
        });
        let audio = Arc::new(Mutex::new(AudioState {
            pcm: VecDeque::new(),
            rate: 48_000,
            channels: 2,
            logged: false,
        }));
        let (ready_tx, ready_rx) = mpsc::channel();
        let frame_stop = Arc::clone(&stop);
        let frame_latest = Arc::clone(&latest);
        let monitor_raw = monitor.0 as usize;
        let frame_thread = thread::spawn(move || {
            let monitor = HMONITOR(monitor_raw as *mut core::ffi::c_void);
            capture_thread(monitor, pixel_width, pixel_height, frame_stop, frame_latest, ready_tx)
        });
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = frame_thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = frame_thread.join();
                return Err(PlatformError::new("capture thread stopped before it started"));
            }
        }
        let audio_stop = Arc::clone(&stop);
        let audio_slot = Arc::clone(&audio);
        let audio_thread = thread::spawn(move || loopback_loop(audio_stop, audio_slot));
        Ok(Self {
            stop,
            latest,
            audio,
            frame_thread: Mutex::new(Some(frame_thread)),
            audio_thread: Mutex::new(Some(audio_thread)),
            info: DisplayInfo {
                width: pixel_width,
                height: pixel_height,
                refresh_millihz: refresh,
            },
            geometry: DisplayGeometry {
                pixel_width,
                pixel_height,
                origin_x: f64::from(placed.left),
                origin_y: f64::from(placed.top),
                point_width: f64::from(native_w),
                point_height: f64::from(native_h),
            },
            fps,
        })
    }

    pub fn wait_frame(&self, timeout: Duration) -> Option<CapturedFrame> {
        let guard = self.latest.frame.lock().ok()?;
        if guard.is_some() {
            return guard.take_frame();
        }
        let (guard, _) = self.latest.cv.wait_timeout(guard, timeout).ok()?;
        guard.take_frame()
    }

    pub fn take_audio(&self) -> Option<PcmChunk> {
        let mut audio = self.audio.lock().ok()?;
        if audio.pcm.is_empty() {
            return None;
        }
        let interleaved: Vec<f32> = audio.pcm.drain(..).collect();
        Some(PcmChunk {
            rate: audio.rate,
            channels: audio.channels,
            interleaved,
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.latest.cv.notify_all();
        if let Ok(mut thread) = self.frame_thread.lock() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
        if let Ok(mut thread) = self.audio_thread.lock() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

trait TakeFrame {
    fn take_frame(self) -> Option<CapturedFrame>;
}

impl TakeFrame for std::sync::MutexGuard<'_, Option<CapturedFrame>> {
    fn take_frame(mut self) -> Option<CapturedFrame> {
        self.take()
    }
}

struct Place {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

fn monitors() -> Result<Vec<HMONITOR>, PlatformError> {
    let mut list = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            HDC(ptr::null_mut()),
            None,
            Some(enum_monitor),
            LPARAM(&mut list as *mut Vec<HMONITOR> as isize),
        )
    };
    if ok.0 == 0 || list.is_empty() {
        return Err(PlatformError::new("no display is connected"));
    }
    Ok(list)
}

unsafe extern "system" fn enum_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<HMONITOR>);
    list.push(monitor);
    BOOL(1)
}

fn monitor_place(monitor: HMONITOR) -> Result<Place, PlatformError> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if ok.0 == 0 {
        return Err(PlatformError::new("display size is unavailable"));
    }
    Ok(Place {
        left: info.monitorInfo.rcMonitor.left,
        top: info.monitorInfo.rcMonitor.top,
        right: info.monitorInfo.rcMonitor.right,
        bottom: info.monitorInfo.rcMonitor.bottom,
    })
}

fn refresh_millihz(monitor: HMONITOR) -> u32 {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if ok.0 == 0 {
        return 0;
    }
    let mut mode = DEVMODEW::default();
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let found = unsafe { EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    if found.0 == 0 {
        return 0;
    }
    mode.dmDisplayFrequency.saturating_mul(1000)
}

fn capture_thread(
    monitor: HMONITOR,
    out_w: u32,
    out_h: u32,
    stop: Arc<AtomicBool>,
    latest: Arc<Latest>,
    ready: mpsc::Sender<Result<(), PlatformError>>,
) {
    let mf = match MfGuard::startup() {
        Ok(mf) => mf,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let started = (|| {
        let (device, context) = d3d_device()?;
        let winrt = winrt_device(&device)?;
        let item = capture_item(monitor)?;
        let size = item.Size().map_err(PlatformError::new)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .or_else(|_| Direct3D11CaptureFramePool::Create(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size))
        .map_err(PlatformError::new)?;
        let session = pool.CreateCaptureSession(&item).map_err(PlatformError::new)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(PlatformError::new)?;
        Ok((device, context, pool, session))
    })();
    let (device, context, pool, session) = match started {
        Ok(opened) => opened,
        Err(err) => {
            let _ = ready.send(Err(err));
            drop(mf);
            return;
        }
    };
    let _ = ready.send(Ok(()));
    pump(&device, &context, &pool, &session, out_w, out_h, &stop, &latest);
    let _ = session.Close();
    let _ = pool.Close();
    drop(mf);
}

fn pump(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    pool: &Direct3D11CaptureFramePool,
    _session: &GraphicsCaptureSession,
    out_w: u32,
    out_h: u32,
    stop: &AtomicBool,
    latest: &Latest,
) {
    let mut staging: Option<ID3D11Texture2D> = None;
    let mut previous = 0u64;
    let mut have_previous = false;
    while !stop.load(Ordering::Relaxed) {
        let mut got = false;
        loop {
            let frame = match pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(_) => break,
            };
            got = true;
            if let Ok(bgra) = copy_frame(device, context, &mut staging, &frame, out_w, out_h) {
                let hash = sample_hash(&bgra, out_w, out_h);
                let changed = if have_previous && hash == previous {
                    Some(0.0)
                } else {
                    Some(1.0)
                };
                previous = hash;
                have_previous = true;
                if let Ok(mut slot) = latest.frame.lock() {
                    *slot = Some(CapturedFrame {
                        bgra,
                        width: out_w,
                        height: out_h,
                        capture_us: mono_us(),
                        changed,
                    });
                }
                latest.cv.notify_all();
            }
            let _ = frame.Close();
        }
        if !got {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), PlatformError> {
    let mut device = None;
    let mut context = None;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE(ptr::null_mut()),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(PlatformError::new)?;
    let device = device.ok_or_else(|| PlatformError::new("Direct3D 11 device did not start"))?;
    let context = context.ok_or_else(|| PlatformError::new("Direct3D 11 context did not start"))?;
    Ok((device, context))
}

fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, PlatformError> {
    let dxgi: IDXGIDevice = device.cast().map_err(PlatformError::new)?;
    let mut raw = ptr::null_mut();
    let hr = unsafe { CreateDirect3D11DeviceFromDXGIDevice(Interface::as_raw(&dxgi), &mut raw) };
    if hr.is_err() || raw.is_null() {
        return Err(PlatformError::new(format!("Direct3D device bridge ({})", hr.0)));
    }
    let inspectable = unsafe { windows::core::IInspectable::from_raw(raw) };
    inspectable.cast().map_err(PlatformError::new)
}

fn capture_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem, PlatformError> {
    let interop = GraphicsCaptureItem::IGraphicsCaptureItemStatics2(|statics| unsafe {
        let raw = Interface::as_raw(statics);
        let mut out = ptr::null_mut();
        let vtbl = &*(*(raw as *mut *const UnknownVtbl));
        let hr = (vtbl.query)(raw, &IID_CAPTURE_INTEROP, &mut out);
        if hr.is_err() || out.is_null() {
            return Err(windows::core::Error::from(hr));
        }
        Ok(out as isize)
    })
    .map_err(PlatformError::new)?;
    unsafe {
        let interop = interop as *mut core::ffi::c_void;
        let vtbl = &*(*(interop as *mut *const CaptureInteropVtbl));
        let mut result = ptr::null_mut();
        let hr = (vtbl.create_for_monitor)(
            interop,
            monitor,
            &<GraphicsCaptureItem as Interface>::IID,
            &mut result,
        );
        (vtbl.base.release)(interop);
        if hr.is_err() || result.is_null() {
            return Err(PlatformError::new(format!("display capture item ({})", hr.0)));
        }
        windows::core::Type::from_abi(result).map_err(PlatformError::new)
    }
}

fn copy_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &mut Option<ID3D11Texture2D>,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    out_w: u32,
    out_h: u32,
) -> Result<Vec<u8>, PlatformError> {
    let surface = frame.Surface().map_err(PlatformError::new)?;
    let texture = texture_from_surface(&surface)?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    ensure_staging(device, staging, desc.Width, desc.Height)?;
    let staging_tex = staging.as_ref().expect("staging texture");
    unsafe {
        context.CopyResource(staging_tex, &texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(staging_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(PlatformError::new)?;
        let bgra = rows_from_mapped(mapped.pData as *const u8, mapped.RowPitch, desc.Width, desc.Height);
        context.Unmap(staging_tex, 0);
        Ok(scale_bgra(&bgra, desc.Width, desc.Height, out_w, out_h))
    }
}

fn texture_from_surface(surface: &IDirect3DSurface) -> Result<ID3D11Texture2D, PlatformError> {
    unsafe {
        let raw = Interface::as_raw(surface);
        let mut access = ptr::null_mut();
        let vtbl = &*(*(raw as *mut *const UnknownVtbl));
        let hr = (vtbl.query)(raw, &IID_DXGI_ACCESS, &mut access);
        if hr.is_err() || access.is_null() {
            return Err(PlatformError::new(format!("capture surface ({})", hr.0)));
        }
        let access_vt = &*(*(access as *mut *const DxgiAccessVtbl));
        let mut texture = ptr::null_mut();
        let hr = (access_vt.get_interface)(access, &ID3D11Texture2D::IID, &mut texture);
        (access_vt.base.release)(access);
        if hr.is_err() || texture.is_null() {
            return Err(PlatformError::new("capture texture is unavailable"));
        }
        Ok(ID3D11Texture2D::from_raw(texture))
    }
}

fn ensure_staging(
    device: &ID3D11Device,
    staging: &mut Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
) -> Result<(), PlatformError> {
    if let Some(current) = staging.as_ref() {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { current.GetDesc(&mut desc) };
        if desc.Width == width && desc.Height == height {
            return Ok(());
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(PlatformError::new)?;
    *staging = texture;
    Ok(())
}

fn rows_from_mapped(data: *const u8, pitch: u32, width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = vec![0u8; row * height as usize];
    if data.is_null() || pitch == 0 {
        return out;
    }
    for y in 0..height as usize {
        let src = unsafe { data.add(y * pitch as usize) };
        let dst = y * row;
        unsafe { ptr::copy_nonoverlapping(src, out[dst..].as_mut_ptr(), row.min(pitch as usize)) };
    }
    out
}

fn loopback_loop(stop: Arc<AtomicBool>, slot: Arc<Mutex<AudioState>>) {
    let _mf = MfGuard::startup();
    if let Err(err) = run_loopback(&stop, &slot) {
        if let Ok(mut audio) = slot.lock() {
            if !audio.logged {
                audio.logged = true;
                eprintln!("system audio unavailable ({err}); continuing with picture only");
            }
        }
    }
}

fn run_loopback(stop: &AtomicBool, slot: &Mutex<AudioState>) -> Result<(), PlatformError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(PlatformError::new)?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(PlatformError::new)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(PlatformError::new)?;
        let mix = client.GetMixFormat().map_err(PlatformError::new)?;
        let described = describe_wave(mix);
        let event = CreateEventW(None, BOOL(0), BOOL(0), PCWSTR::null()).map_err(PlatformError::new)?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                2_000_000,
                0,
                mix,
                None,
            )
            .map_err(PlatformError::new)?;
        CoTaskMemFree(Some(mix.cast()));
        let capture = client.GetService::<IAudioCaptureClient>().map_err(PlatformError::new)?;
        client.Start().map_err(PlatformError::new)?;
        while !stop.load(Ordering::Relaxed) {
            let packet = capture.GetNextPacketSize().unwrap_or(0);
            if packet == 0 {
                let _ = WaitForSingleObject(event, 10);
                thread::sleep(Duration::from_millis(4));
                continue;
            }
            let mut data = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                break;
            }
            if frames > 0 && !data.is_null() && flags & 2 == 0 {
                push_pcm(slot, &described, data, frames);
            }
            capture.ReleaseBuffer(frames).ok();
        }
        client.Stop().ok();
        windows::Win32::Foundation::CloseHandle(event).ok();
        Ok(())
    }
}

struct WaveInfo {
    rate: u32,
    channels: u16,
    bits: u16,
    is_float: bool,
}

fn describe_wave(mix: *mut WAVEFORMATEX) -> WaveInfo {
    let format = unsafe { ptr::read_unaligned(mix) };
    let mut is_float = format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT;
    if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE && format.cbSize >= 22 {
        let ext = unsafe { ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE) };
        is_float = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.SubFormat)) } == MFAudioFormat_Float;
    }
    WaveInfo {
        rate: format.nSamplesPerSec.max(1),
        channels: format.nChannels.max(1),
        bits: format.wBitsPerSample,
        is_float,
    }
}

fn push_pcm(slot: &Mutex<AudioState>, wave: &WaveInfo, data: *mut u8, frames: u32) {
    let channels = wave.channels as usize;
    let mut samples = Vec::with_capacity(frames as usize * channels);
    unsafe {
        if wave.is_float && wave.bits == 32 {
            let src = std::slice::from_raw_parts(data as *const f32, frames as usize * channels);
            samples.extend_from_slice(src);
        } else if wave.bits == 16 || wave.w_format_is_pcm() {
            let src = std::slice::from_raw_parts(data as *const i16, frames as usize * channels);
            samples.extend(src.iter().map(|sample| *sample as f32 / 32768.0));
        } else {
            return;
        }
    }
    let stereo = downmix_stereo(&samples, channels);
    let resampled = resample_linear(&stereo, wave.rate, 48_000, 2);
    let Ok(mut audio) = slot.lock() else {
        return;
    };
    audio.rate = 48_000;
    audio.channels = 2;
    audio.pcm.extend(resampled);
    let cap = 48_000usize * 2;
    if audio.pcm.len() > cap {
        let extra = audio.pcm.len() - cap;
        let extra = extra / 2 * 2;
        audio.pcm.drain(..extra);
    }
}

impl WaveInfo {
    fn w_format_is_pcm(&self) -> bool {
        self.bits == 16
    }
}

fn downmix_stereo(input: &[f32], channels: usize) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    if channels == 2 {
        return input.to_vec();
    }
    let frames = input.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);
    for frame in 0..frames {
        let base = frame * channels;
        if channels == 1 {
            let sample = input[base];
            out.push(sample);
            out.push(sample);
        } else {
            out.push(input[base]);
            out.push(input[base + 1]);
        }
    }
    out
}

fn resample_linear(input: &[f32], in_rate: u32, out_rate: u32, channels: usize) -> Vec<f32> {
    if in_rate == out_rate || in_rate == 0 {
        return input.to_vec();
    }
    let frames = input.len() / channels.max(1);
    if frames == 0 {
        return Vec::new();
    }
    let out_frames = ((frames as u64 * u64::from(out_rate)) / u64::from(in_rate)).max(1) as usize;
    let mut out = Vec::with_capacity(out_frames * channels);
    for frame in 0..out_frames {
        let position = frame as f64 * f64::from(in_rate) / f64::from(out_rate);
        let index = position.floor() as usize;
        let frac = (position - index as f64) as f32;
        let a = index.min(frames - 1);
        let b = (index + 1).min(frames - 1);
        for channel in 0..channels {
            let left = input[a * channels + channel];
            let right = input[b * channels + channel];
            out.push(left + (right - left) * frac);
        }
    }
    out
}


