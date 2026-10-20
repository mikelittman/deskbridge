use std::time::{Duration, Instant};

use deskbridge_platform_macos::{hardware_decode_codecs, CursorDraw, Decoder, Renderer};

use crate::error::PlatformError;
use deskbridge_proto::{CodecConfig, CodecId, Control, CursorPos, Input, MouseButton};
use deskbridge_session::{
    agent_available, connect, mono_us, parse_fingerprint, parse_public_key_line, ClientAuth, KeyChoice,
    PairedClient,
};
use tokio::sync::mpsc::error::TryRecvError;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::{parse_bitrate, resolve_targets, Latency};

pub fn view(args: Vec<String>) -> Result<(), PlatformError> {
    let args = parse_view(&args)?;
    let chosen = match &args.key {
        Some(line) => Some(parse_public_key_line(line).map_err(PlatformError::new)?.blob),
        None => None,
    };
    if args.code.is_none() && chosen.is_none() && !agent_available() {
        return Err(PlatformError::new(
            "view needs --code from the host, or an SSH agent with a key the host authorizes",
        ));
    }
    let auth = ClientAuth {
        code: args.code.clone(),
        keys: if chosen.is_some() || agent_available() {
            KeyChoice::Agent { chosen }
        } else {
            KeyChoice::PairingCodeOnly
        },
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(PlatformError::new)?;
    let client = runtime.block_on(connect_first(
        &args.addrs,
        &auth,
        args.fingerprint,
        hardware_decode_codecs(),
    ))?;
    println!(
        "paired with {}",
        deskbridge_session::hex(&client.fingerprint)
    );
    let event_loop = EventLoop::new().map_err(PlatformError::new)?;
    let mut app = Viewer {
        runtime,
        client,
        window: None,
        renderer: Renderer::new().map_err(PlatformError::new)?,
        grabbed: false,
        focused: true,
        modifiers: ModifiersState::empty(),
        pointer: None,
        local_cursor: None,
        host_cursor: CursorPos { x: 0, y: 0, visible: false },
        hotspot: (1, 1),
        config: None,
        decoder: Decoder::new(),
        picture: None,
        codec: None,
        latency: Latency::new(),
        dirty: true,
        ended: None,
        announced: false,
    };
    event_loop.run_app(&mut app).map_err(PlatformError::new)?;
    Ok(())
}

struct ViewArgs {
    addrs: Vec<std::net::SocketAddr>,
    code: Option<String>,
    key: Option<String>,
    fingerprint: Option<[u8; 32]>,
}

async fn connect_first(
    addrs: &[std::net::SocketAddr],
    auth: &ClientAuth,
    fingerprint: Option<[u8; 32]>,
    codecs: Vec<deskbridge_proto::CodecId>,
) -> Result<PairedClient, PlatformError> {
    let mut failures = Vec::new();
    for addr in addrs {
        println!("connecting to {addr}");
        match connect(*addr, auth, fingerprint, codecs.clone()).await {
            Ok(client) => return Ok(client),
            Err(err) if err.is_fatal() => return Err(PlatformError::new(err)),
            Err(err) => {
                eprintln!("{addr}: {err}");
                failures.push(format!("{addr}: {err}"));
            }
        }
    }
    Err(PlatformError::new(format!(
        "could not reach the host\n{}\nThe host must be running. A host started without --listen only accepted 127.0.0.1; restart it with --listen [::]:9787",
        failures.join("\n")
    )))
}

fn parse_view(args: &[String]) -> Result<ViewArgs, PlatformError> {
    let mut addr = None;
    let mut code = None;
    let mut key = None;
    let mut fingerprint = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--code" => {
                index += 1;
                code = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| PlatformError::new("--code needs the pairing code"))?,
                );
            }
            "--key" => {
                index += 1;
                if key.is_some() {
                    return Err(PlatformError::new("view takes one --key"));
                }
                key = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| PlatformError::new("--key needs an ssh-ed25519 public key"))?,
                );
            }
            "--fingerprint" => {
                index += 1;
                let text = args
                    .get(index)
                    .ok_or_else(|| PlatformError::new("--fingerprint needs 64 hex characters"))?;
                fingerprint = Some(parse_fingerprint(text).map_err(PlatformError::new)?);
            }
            "--bitrate" => {
                index += 1;
                let _ = parse_bitrate(
                    args.get(index)
                        .ok_or_else(|| PlatformError::new("--bitrate needs a value"))?,
                )?;
            }
            other if other.starts_with('-') => {
                return Err(PlatformError::new(format!("unknown view option {other}")));
            }
            other => addr = Some(resolve_targets(other)?),
        }
        index += 1;
    }
    Ok(ViewArgs {
        addrs: addr.ok_or_else(|| PlatformError::new("view needs an address, for example 127.0.0.1"))?,
        code,
        key,
        fingerprint,
    })
}

struct Viewer {
    runtime: tokio::runtime::Runtime,
    client: PairedClient,
    window: Option<Window>,
    renderer: Renderer,
    grabbed: bool,
    focused: bool,
    modifiers: ModifiersState,
    pointer: Option<(f64, f64)>,
    local_cursor: Option<(i32, i32)>,
    host_cursor: CursorPos,
    hotspot: (i32, i32),
    config: Option<CodecConfig>,
    decoder: Decoder,
    picture: Option<apple_cf::iosurface::IOSurface>,
    codec: Option<CodecId>,
    latency: Latency,
    dirty: bool,
    ended: Option<String>,
    announced: bool,
}

pub(crate) fn session_end_line(reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "host stopped responding".to_string()
    } else {
        format!("host ended the session ({reason})")
    }
}

impl Viewer {
    fn poll(&mut self) -> bool {
        if self.ended.is_some() {
            return false;
        }
        let mut dirty = self.dirty;
        self.dirty = false;
        loop {
            match self.client.from_peer.try_recv() {
                Ok(message) => self.apply_control(message, &mut dirty),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let reason = self
                        .client
                        .close_reason()
                        .unwrap_or_else(|| "connection closed".to_string());
                    self.note_end(reason);
                    break;
                }
            }
        }
        if let Some(reason) = self.client.close_reason() {
            self.note_end(reason);
        }
        if self.ended.is_some() {
            return true;
        }
        if let Some(frame) = self.client.frames.try_recv() {
            if let Some(config) = self.config.clone() {
                match self.decoder.decode(&config, &frame) {
                    Ok(Some(surface)) => {
                        self.latency.note_frame(
                            frame.id,
                            frame.capture_us,
                            frame.payload.len() as u32,
                        );
                        self.picture = Some(surface);
                        dirty = true;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!("decode: {err}");
                        self.client
                            .send_control(Control::KeyframeRequest { newest_seen: frame.id });
                    }
                }
            } else {
                self.client
                    .send_control(Control::KeyframeRequest { newest_seen: frame.id });
            }
        }
        if self.latency.due_ping() {
            self.client
                .send_control(Control::Ping { client_us: mono_us() });
            dirty = true;
        }
        dirty
    }

    fn apply_control(&mut self, message: Control, dirty: &mut bool) {
        match message {
            Control::CodecConfig(config) => {
                self.codec = Some(config.codec);
                self.config = Some(config);
            }
            Control::CursorShape(shape) => {
                self.hotspot = (i32::from(shape.hotspot_x), i32::from(shape.hotspot_y));
                self.renderer.set_cursor_bgra(
                    shape.width,
                    shape.height,
                    shape.hotspot_x,
                    shape.hotspot_y,
                    &shape.bgra,
                );
                *dirty = true;
            }
            Control::CursorPos(pos) => {
                self.host_cursor = pos;
                *dirty = true;
            }
            Control::Pong { client_us, host_us } => self.latency.note_pong(client_us, host_us),
            _ => {}
        }
    }

    fn note_end(&mut self, reason: String) {
        if self.ended.is_none() {
            self.ended = Some(reason);
        }
    }

    fn release_grab(&mut self) {
        if self.grabbed {
            self.grabbed = false;
            if let Some(window) = &self.window {
                window.set_cursor_visible(true);
            }
            self.dirty = true;
        }
    }

    fn grab_at(&mut self, x: f64, y: f64) {
        self.grabbed = true;
        if let Some(window) = &self.window {
            window.set_cursor_visible(false);
        }
        self.send_pointer(x, y);
    }

    fn send_pointer(&mut self, x: f64, y: f64) {
        self.pointer = Some((x, y));
        if let Some((px, py)) = self.renderer.map_pointer(x as f32, y as f32) {
            self.local_cursor = Some((px, py));
            self.client
                .send_control(Control::Input(Input::MouseMove { x: px, y: py }));
            self.dirty = true;
        }
    }

    fn draw(&mut self) {
        let cursor = if self.grabbed {
            self.local_cursor.map(|(x, y)| CursorDraw {
                x,
                y,
                hotspot_x: self.hotspot.0,
                hotspot_y: self.hotspot.1,
            })
        } else if self.host_cursor.visible {
            Some(CursorDraw {
                x: self.host_cursor.x,
                y: self.host_cursor.y,
                hotspot_x: self.hotspot.0,
                hotspot_y: self.hotspot.1,
            })
        } else {
            None
        };
        let hud = self.latency.line(self.codec);
        if let Err(err) = self.renderer.present(self.picture.as_ref(), cursor, &hud) {
            eprintln!("present: {err}");
        }
    }
}

impl ApplicationHandler for Viewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Deskbridge")
                    .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0)),
            )
            .expect("window");
        let scale = window.scale_factor();
        let size = window.inner_size();
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                self.renderer.attach(
                    appkit.ns_view.as_ptr(),
                    scale,
                    f64::from(size.width),
                    f64::from(size.height),
                );
            }
        }
        self.window = Some(window);
        self.dirty = true;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    self.release_grab();
                }
            }
            WindowEvent::Resized(size) => {
                let scale = self.window.as_ref().map(|window| window.scale_factor()).unwrap_or(1.0);
                self.renderer
                    .resize(f64::from(size.width), f64::from(size.height), scale);
                self.dirty = true;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    self.renderer
                        .resize(f64::from(size.width), f64::from(size.height), scale_factor);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                if self.modifiers.control_key() && self.modifiers.alt_key() {
                    self.release_grab();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.grabbed && self.focused {
                    self.send_pointer(position.x, position.y);
                } else {
                    self.pointer = Some((position.x, position.y));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if !self.focused {
                    return;
                }
                if state == ElementState::Pressed && !self.grabbed {
                    if let Some((x, y)) = self.pointer {
                        self.grab_at(x, y);
                    }
                }
                if self.grabbed {
                    if let Some(button) = mouse_button(button) {
                        self.client.send_control(Control::Input(Input::MouseButton {
                            button,
                            down: state == ElementState::Pressed,
                        }));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.grabbed && self.focused => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as i32, y as i32),
                    MouseScrollDelta::PixelDelta(pos) => (pos.x as i32, pos.y as i32),
                };
                self.client
                    .send_control(Control::Input(Input::Wheel { dx, dy }));
            }
            WindowEvent::KeyboardInput { event, .. } if self.focused => {
                let release = self.modifiers.control_key() && self.modifiers.alt_key();
                let is_chord = matches!(
                    event.physical_key,
                    PhysicalKey::Code(KeyCode::ControlLeft)
                        | PhysicalKey::Code(KeyCode::ControlRight)
                        | PhysicalKey::Code(KeyCode::AltLeft)
                        | PhysicalKey::Code(KeyCode::AltRight)
                );
                if release && is_chord {
                    self.release_grab();
                    return;
                }
                if !self.grabbed {
                    return;
                }
                let Some(code) = event.physical_key.to_scancode() else {
                    return;
                };
                let code = u16::try_from(code).unwrap_or(0);
                self.client.send_control(Control::Input(Input::Key {
                    code,
                    down: event.state == ElementState::Pressed,
                    modifiers: crate::modifiers_from_winit(self.modifiers),
                }));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let dirty = self.poll();
        if self.announced {
            event_loop.exit();
            return;
        }
        if let Some(reason) = self.ended.clone() {
            println!("{}", session_end_line(&reason));
            self.announced = true;
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(4),
        ));
        if dirty {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_view, session_end_line};

    #[test]
    fn view_takes_a_key_without_a_pairing_code() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBERERERERERERERERERERERERERERERERERERERERER laptop";
        let args = parse_view(&["127.0.0.1".into(), "--key".into(), line.into()]).unwrap();
        assert_eq!(args.key.as_deref(), Some(line));
        assert!(args.code.is_none());
        assert!(parse_view(&["127.0.0.1".into(), "--key".into(), line.into(), "--key".into(), line.into()]).is_err());
    }

    #[test]
    fn a_timeout_says_the_host_stopped_responding() {
        assert_eq!(session_end_line("timed out"), "host stopped responding");
        assert_eq!(
            session_end_line("closed by peer: closed (code 0)"),
            "host ended the session (closed by peer: closed (code 0))"
        );
    }
}

fn mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = &self.runtime;
    }
}
