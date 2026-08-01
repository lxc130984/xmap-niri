//! Niri IPC connection: a dedicated blocking thread that keeps the shared
//! snapshot up to date and nudges the UI thread via a channel.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use calloop::channel::Sender;
use log::{debug, warn};
use niri_ipc::socket::Socket;
use niri_ipc::state::{EventStreamState, EventStreamStatePart};
use niri_ipc::{Event, Request, Response};

use crate::config::Config;

/// Messages sent from helper threads to the UI event loop.
#[derive(Debug, Clone, Copy)]
pub enum UiMsg {
    /// Niri state changed; `show` requests the transient minimap pop-up.
    StateChanged { show: bool },
    /// The config file changed.
    ConfigReload,
}

/// The complete state we need to render, shared between the IPC thread
/// (writer) and the UI thread (reader).
#[derive(Default)]
pub struct Snapshot {
    pub state: EventStreamState,
    pub outputs: HashMap<String, niri_ipc::Output>,
}

pub type Shared = Arc<RwLock<Snapshot>>;

/// Spawn the IPC thread. It reconnects automatically if niri restarts.
pub fn spawn_ipc_thread(
    shared: Shared,
    config: Arc<RwLock<Config>>,
    tx: Sender<UiMsg>,
) -> Result<()> {
    thread::Builder::new()
        .name("nirimap-ipc".into())
        .spawn(move || {
            while let Err(err) = ipc_loop(&shared, &config, &tx) {
                warn!("niri IPC connection lost: {err:#}; reconnecting in 2s...");
                thread::sleep(Duration::from_secs(2));
            }
        })?;
    Ok(())
}

fn ipc_loop(shared: &Shared, config: &Arc<RwLock<Config>>, tx: &Sender<UiMsg>) -> Result<()> {
    let mut socket = Socket::connect().context("failed to connect to the niri IPC socket (is niri running?)")?;

    let workspaces = match socket.send(Request::Workspaces)? {
        Ok(Response::Workspaces(w)) => w,
        Ok(other) => bail!("unexpected reply to Workspaces: {other:?}"),
        Err(msg) => bail!("niri error for Workspaces: {msg}"),
    };
    let windows = match socket.send(Request::Windows)? {
        Ok(Response::Windows(w)) => w,
        Ok(other) => bail!("unexpected reply to Windows: {other:?}"),
        Err(msg) => bail!("niri error for Windows: {msg}"),
    };
    let outputs = match socket.send(Request::Outputs)? {
        Ok(Response::Outputs(o)) => o,
        Ok(other) => bail!("unexpected reply to Outputs: {other:?}"),
        Err(msg) => bail!("niri error for Outputs: {msg}"),
    };

    {
        let mut snap = shared.write().unwrap_or_else(|e| e.into_inner());
        snap.state.workspaces.workspaces = workspaces.into_iter().map(|w| (w.id, w)).collect();
        snap.state.windows.windows = windows.into_iter().map(|w| (w.id, w)).collect();
        snap.outputs = outputs;
    }
    tx.send(UiMsg::StateChanged { show: true }).ok();
    debug!("connected to niri; snapshot loaded");

    match socket.send(Request::EventStream)? {
        Ok(Response::Handled) => {}
        Ok(other) => bail!("unexpected reply to EventStream: {other:?}"),
        Err(msg) => bail!("niri rejected EventStream: {msg}"),
    }

    let mut read_event = socket.read_events();
    loop {
        let event = read_event().context("failed to read the niri event stream")?;
        let show = {
            let mut snap = shared.write().unwrap_or_else(|e| e.into_inner());
            let cfg = config.read().unwrap_or_else(|e| e.into_inner());
            let show = should_show(&event, &snap.state, &cfg);
            snap.state.apply(event);
            show
        };
        tx.send(UiMsg::StateChanged { show }).ok();
    }
}

/// Decide whether this event should pop the transient minimap up.
fn should_show(event: &Event, state: &EventStreamState, config: &Config) -> bool {
    if config.behavior.always_visible || config.behavior.show_for_floating_windows {
        return true;
    }
    match event {
        Event::WindowOpenedOrChanged { window } => !window.is_floating,
        Event::WindowFocusChanged { id: Some(id) } => state
                .windows
                .windows
                .get(id)
                .map(|w| !w.is_floating)
                .unwrap_or(true),
        Event::WindowFocusChanged { id: None } => true,
        _ => true,
    }
}
