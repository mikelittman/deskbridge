//! One host process, many viewers, one capture.
//!
//! The accept task is separate from frame fan-out. Selecting `accept_one`
//! beside the frame loop would cancel a handshake that is waiting on an SSH agent.

use std::io::Write;
use std::time::Duration;

use deskbridge_platform_macos::{Host, HostOptions, Outbound};
use deskbridge_proto::{CodecId, Control, Input, VideoFrame};
use deskbridge_session::{
    accept_one, close_endpoint, AuthorizedKey, Endpoint, Identity, PairedHost, ViewerProof,
};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::daemon;
use crate::error::PlatformError;

pub async fn serve(
    endpoint: Endpoint,
    identity: Identity,
    codec: CodecId,
    authorized: Vec<AuthorizedKey>,
    display_index: usize,
    bitrate: u32,
    inject: bool,
    resolution: Option<(u32, u32)>,
) {
    host_line("waiting for a viewer");
    let (join_tx, mut join_rx) = mpsc::unbounded_channel();
    let (notes_tx, mut notes_rx) = mpsc::unbounded_channel();
    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel();
    let accept_endpoint = endpoint.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            match accept_one(&accept_endpoint, &identity, vec![codec], &authorized).await {
                Ok(paired) => {
                    if join_tx.send(paired).is_err() {
                        break;
                    }
                }
                Err(err) if err.to_string().contains("endpoint closed") => break,
                Err(err) => eprintln!("deskbridge: {err}"),
            }
        }
    });

    let mut shutdown = std::pin::pin!(shutdown_requested());
    let mut viewers: Vec<LiveViewer> = Vec::new();
    let mut next_id = 1u64;
    let mut running: Option<RunningCapture> = None;
    let mut next_generation = 1u64;
    let mut live_generation: Option<u64> = None;
    let mut picture = SharedPicture::default();
    let plan = CapturePlan {
        display_index,
        bitrate,
        inject,
        codec,
        resolution,
    };

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                host_line("stopping");
                break;
            }
            joined = join_rx.recv() => {
                let Some(paired) = joined else {
                    host_line("stopping");
                    break;
                };
                admit(
                    paired,
                    &mut viewers,
                    &mut next_id,
                    &mut running,
                    &mut live_generation,
                    &mut picture,
                    &notes_tx,
                    &bridge_tx,
                    &plan,
                    &mut next_generation,
                );
            }
            note = notes_rx.recv() => {
                let Some(note) = note else { break };
                on_note(
                    note,
                    &mut viewers,
                    &mut running,
                    &mut live_generation,
                    &mut picture,
                    &plan,
                    &mut next_generation,
                    &bridge_tx,
                );
            }
            message = bridge_rx.recv() => {
                match message {
                    Some(BridgeMsg::Control { generation, control })
                        if live_generation == Some(generation) =>
                    {
                        publish_control(&viewers, &mut picture, control);
                    }
                    Some(BridgeMsg::Frame { generation, frame })
                        if live_generation == Some(generation) =>
                    {
                        if publish_frame(&mut viewers, &mut picture, frame) {
                            if let Some(capture) = running.as_ref() {
                                capture.host.request_keyframe();
                            }
                        }
                    }
                    Some(BridgeMsg::Ended { generation }) if live_generation == Some(generation) => {
                        eprintln!("deskbridge: capture stopped");
                        release_capture(&mut running, &mut live_generation, &mut picture);
                        close_viewers(&mut viewers).await;
                        host_line("waiting for a viewer");
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    close_endpoint(&endpoint);
    accept_task.abort();
    close_viewers(&mut viewers).await;
    release_capture(&mut running, &mut live_generation, &mut picture);
    let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
}

struct CapturePlan {
    display_index: usize,
    bitrate: u32,
    inject: bool,
    codec: CodecId,
    resolution: Option<(u32, u32)>,
}

struct RunningCapture {
    host: Host,
    pump: JoinHandle<()>,
    limit: Option<(u32, u32)>,
}

enum BridgeMsg {
    Control { generation: u64, control: Control },
    Frame { generation: u64, frame: VideoFrame },
    Ended { generation: u64 },
}

struct LiveViewer {
    id: u64,
    paired: PairedHost,
    reader: JoinHandle<()>,
    synced: bool,
    resolution: Option<(u32, u32)>,
}

enum FromViewer {
    Input(Input),
    Keyframe,
    Left,
}

struct ViewerNote {
    id: u64,
    event: FromViewer,
}

#[derive(Default)]
struct SharedPicture {
    display: Option<Control>,
    cursor_shape: Option<Control>,
    codec_config: Option<Control>,
    cursor_pos: Option<Control>,
    keyframe: Option<VideoFrame>,
    keyframe_is_latest: bool,
}

impl SharedPicture {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn note_control(&mut self, message: &Control) {
        match message {
            Control::DisplayInfo(_) => self.display = Some(message.clone()),
            Control::CursorShape(_) => self.cursor_shape = Some(message.clone()),
            Control::CodecConfig(_) => self.codec_config = Some(message.clone()),
            Control::CursorPos(_) => self.cursor_pos = Some(message.clone()),
            _ => {}
        }
    }

    /// Sends the latest setup, and the current keyframe when one is still valid.
    /// Returns whether that keyframe was sent.
    fn replay(&self, paired: &PairedHost) -> bool {
        for message in [
            &self.display,
            &self.cursor_shape,
            &self.codec_config,
            &self.cursor_pos,
        ]
        .into_iter()
        .flatten()
        {
            paired.send_control(message.clone());
        }
        if self.keyframe_is_latest {
            if let Some(frame) = &self.keyframe {
                return paired.offer_frame(frame.clone());
            }
        }
        false
    }
}

fn admit(
    mut paired: PairedHost,
    viewers: &mut Vec<LiveViewer>,
    next_id: &mut u64,
    running: &mut Option<RunningCapture>,
    live_generation: &mut Option<u64>,
    picture: &mut SharedPicture,
    notes_tx: &UnboundedSender<ViewerNote>,
    bridge_tx: &UnboundedSender<BridgeMsg>,
    plan: &CapturePlan,
    next_generation: &mut u64,
) {
    if !paired.peer_codecs.contains(&plan.codec) {
        eprintln!(
            "deskbridge: a viewer cannot decode {}. This host encodes {} for every viewer.",
            plan.codec.name(),
            plan.codec.name()
        );
        tokio::spawn(async move { paired.shutdown().await });
        return;
    }
    let mut requests: Vec<Option<(u32, u32)>> = viewers.iter().map(|viewer| viewer.resolution).collect();
    requests.push(paired.resolution);
    let aligned = ensure_capture(
        &requests,
        viewers,
        running,
        live_generation,
        picture,
        plan,
        next_generation,
        bridge_tx.clone(),
    );
    if !aligned {
        tokio::spawn(async move { paired.shutdown().await });
        return;
    }
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    share_capture(running, viewers, Some(&paired));
    let synced = picture.replay(&paired);
    if !synced {
        if let Some(capture) = running.as_ref() {
            capture.host.request_keyframe();
        }
    }
    let reader = watch_viewer(id, &mut paired, notes_tx.clone());
    let proof = paired.viewer.clone();
    let resolution = paired.resolution;
    let limit = running.as_ref().and_then(|capture| capture.limit);
    viewers.push(LiveViewer {
        id,
        paired,
        reader,
        synced,
        resolution,
    });
    host_line(paired_line(&proof, viewers.len(), plan.codec, limit));
}

fn on_note(
    note: ViewerNote,
    viewers: &mut Vec<LiveViewer>,
    running: &mut Option<RunningCapture>,
    live_generation: &mut Option<u64>,
    picture: &mut SharedPicture,
    plan: &CapturePlan,
    next_generation: &mut u64,
    bridge_tx: &UnboundedSender<BridgeMsg>,
) {
    match note.event {
        FromViewer::Input(input) => {
            if let Some(capture) = running.as_ref() {
                capture.host.inject(input);
            }
        }
        FromViewer::Keyframe => {
            if let Some(capture) = running.as_ref() {
                capture.host.request_keyframe();
            }
        }
        FromViewer::Left => {
            let Some(pos) = viewers.iter().position(|viewer| viewer.id == note.id) else {
                return;
            };
            let viewer = viewers.remove(pos);
            viewer.reader.abort();
            drop(viewer.paired);
            if viewers.is_empty() {
                release_capture(running, live_generation, picture);
                host_line("viewer left. waiting for a viewer");
            } else {
                let requests: Vec<Option<(u32, u32)>> =
                    viewers.iter().map(|viewer| viewer.resolution).collect();
                ensure_capture(
                    &requests,
                    viewers,
                    running,
                    live_generation,
                    picture,
                    plan,
                    next_generation,
                    bridge_tx.clone(),
                );
                share_capture(running, viewers, None);
                let count = viewers.len();
                host_line(format!("viewer left ({count} still connected)"));
            }
        }
    }
}

fn watch_viewer(id: u64, paired: &mut PairedHost, notes_tx: UnboundedSender<ViewerNote>) -> JoinHandle<()> {
    let mut inbound = paired.take_inbound();
    tokio::spawn(async move {
        loop {
            let event = match inbound.recv().await {
                Some(Control::Input(input)) => FromViewer::Input(input),
                Some(Control::KeyframeRequest { .. }) => FromViewer::Keyframe,
                Some(_) => continue,
                None => {
                    let _ = notes_tx.send(ViewerNote {
                        id,
                        event: FromViewer::Left,
                    });
                    break;
                }
            };
            if notes_tx.send(ViewerNote { id, event }).is_err() {
                break;
            }
        }
    })
}

fn publish_control(viewers: &[LiveViewer], picture: &mut SharedPicture, message: Control) {
    picture.note_control(&message);
    for viewer in viewers {
        viewer.paired.send_control(message.clone());
    }
}

/// Returns true when a viewer missed this frame and needs a keyframe to resync.
fn publish_frame(viewers: &mut [LiveViewer], picture: &mut SharedPicture, frame: VideoFrame) -> bool {
    let mut behind = false;
    if frame.keyframe {
        picture.keyframe = Some(frame.clone());
        picture.keyframe_is_latest = true;
        for viewer in viewers.iter_mut() {
            if viewer.paired.offer_frame(frame.clone()) {
                viewer.synced = true;
            } else {
                viewer.synced = false;
                behind = true;
            }
        }
    } else {
        picture.keyframe_is_latest = false;
        for viewer in viewers.iter_mut() {
            if !viewer.synced {
                continue;
            }
            if !viewer.paired.offer_frame(frame.clone()) {
                viewer.synced = false;
                behind = true;
            }
        }
    }
    behind
}

/// The picture fits inside the host ceiling and every viewer's request.
/// A viewer that does not ask leaves the limit unchanged.
fn picture_limit(host: Option<(u32, u32)>, requests: &[Option<(u32, u32)>]) -> Option<(u32, u32)> {
    let mut limit = host;
    for request in requests {
        if let Some((width, height)) = *request {
            limit = Some(match limit {
                Some((limit_w, limit_h)) => (limit_w.min(width), limit_h.min(height)),
                None => (width, height),
            });
        }
    }
    limit
}

fn ensure_capture(
    requests: &[Option<(u32, u32)>],
    viewers: &mut [LiveViewer],
    running: &mut Option<RunningCapture>,
    live_generation: &mut Option<u64>,
    picture: &mut SharedPicture,
    plan: &CapturePlan,
    next_generation: &mut u64,
    bridge_tx: UnboundedSender<BridgeMsg>,
) -> bool {
    let limit = picture_limit(plan.resolution, requests);
    if running.as_ref().map(|capture| capture.limit) == Some(limit) {
        return true;
    }
    let previous = running.as_ref().map(|capture| capture.limit);
    if running.is_some() {
        release_capture(running, live_generation, picture);
        for viewer in viewers.iter_mut() {
            viewer.synced = false;
        }
    }
    match start_capture(plan, limit, next_generation, live_generation, bridge_tx.clone()) {
        Ok(capture) => {
            *running = Some(capture);
            true
        }
        Err(err) => {
            eprintln!("deskbridge: {err}");
            if let Some(previous) = previous {
                if let Ok(capture) =
                    start_capture(plan, previous, next_generation, live_generation, bridge_tx)
                {
                    *running = Some(capture);
                    return true;
                }
            }
            false
        }
    }
}

fn share_capture(running: &Option<RunningCapture>, viewers: &[LiveViewer], extra: Option<&PairedHost>) {
    let Some(capture) = running.as_ref() else {
        return;
    };
    for viewer in viewers {
        viewer.paired.share_backlog(capture.host.video_backlog());
    }
    if let Some(extra) = extra {
        extra.share_backlog(capture.host.video_backlog());
    }
    capture
        .host
        .set_viewer_count(viewers.len() + usize::from(extra.is_some()));
}

fn start_capture(
    plan: &CapturePlan,
    limit: Option<(u32, u32)>,
    next_generation: &mut u64,
    live_generation: &mut Option<u64>,
    bridge_tx: UnboundedSender<BridgeMsg>,
) -> Result<RunningCapture, PlatformError> {
    let generation = *next_generation;
    *next_generation = next_generation.saturating_add(1);
    *live_generation = Some(generation);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = Host::spawn(
        HostOptions {
            display_index: plan.display_index,
            bitrate: plan.bitrate,
            inject: plan.inject,
            codec: plan.codec,
            resolution: limit,
        },
        tx,
    )
    .map_err(PlatformError::new)?;
    let pump = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let event = match message {
                Outbound::Control(control) => BridgeMsg::Control { generation, control },
                Outbound::Frame(frame) => BridgeMsg::Frame { generation, frame },
            };
            if bridge_tx.send(event).is_err() {
                return;
            }
        }
        let _ = bridge_tx.send(BridgeMsg::Ended { generation });
    });
    Ok(RunningCapture { host, pump, limit })
}

fn release_capture(
    running: &mut Option<RunningCapture>,
    live_generation: &mut Option<u64>,
    picture: &mut SharedPicture,
) {
    *live_generation = None;
    if let Some(running) = running.take() {
        drop(running.host);
        running.pump.abort();
    }
    picture.clear();
}

async fn close_viewers(viewers: &mut Vec<LiveViewer>) {
    let taken = std::mem::take(viewers);
    let mut closing = Vec::with_capacity(taken.len());
    for viewer in taken {
        viewer.reader.abort();
        closing.push(tokio::spawn(async move { viewer.paired.shutdown().await }));
    }
    for task in closing {
        let _ = task.await;
    }
}

fn host_line(text: impl std::fmt::Display) {
    println!("{text}");
    let _ = std::io::stdout().flush();
}

fn paired_line(
    proof: &ViewerProof,
    count: usize,
    codec: CodecId,
    limit: Option<(u32, u32)>,
) -> String {
    let how = match proof {
        ViewerProof::PairingCode => "pairing code".to_string(),
        ViewerProof::SshKey { comment } if comment.is_empty() => "ssh key".to_string(),
        ViewerProof::SshKey { comment } => format!("ssh key ({comment})"),
    };
    let noun = if count == 1 { "viewer" } else { "viewers" };
    let size = match limit {
        Some((width, height)) => format!(", picture at most {width}x{height}"),
        None => String::new(),
    };
    format!("paired by {how} ({count} {noun}){size}, encoding {}", codec.name())
}

async fn shutdown_requested() {
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut ctrl_c_ok = true;
    #[cfg(unix)]
    let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => Some(signal),
        Err(err) => {
            eprintln!("deskbridge: stop signal is unavailable ({err})");
            None
        }
    };
    loop {
        tokio::select! {
            result = &mut ctrl_c, if ctrl_c_ok => {
                match result {
                    Ok(()) => return,
                    Err(err) => {
                        eprintln!("deskbridge: ctrl-c is unavailable ({err})");
                        ctrl_c_ok = false;
                    }
                }
            }
            _ = async {
                #[cfg(unix)]
                {
                    if let Some(signal) = terminate.as_mut() {
                        signal.recv().await;
                        return;
                    }
                }
                std::future::pending::<()>().await
            } => return,
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if daemon::is_child() && daemon::stop_requested() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::picture_limit;

    #[test]
    fn a_viewer_request_tightens_the_host_ceiling() {
        assert_eq!(picture_limit(None, &[None]), None);
        assert_eq!(picture_limit(Some((1920, 1080)), &[None]), Some((1920, 1080)));
        assert_eq!(picture_limit(None, &[Some((1280, 720))]), Some((1280, 720)));
        assert_eq!(
            picture_limit(Some((3840, 2160)), &[Some((1280, 720))]),
            Some((1280, 720))
        );
        assert_eq!(
            picture_limit(None, &[None, Some((1920, 1080)), Some((1280, 720))]),
            Some((1280, 720))
        );
    }
}
