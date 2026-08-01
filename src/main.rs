mod app;
mod config;
mod ipc;
mod layout;
mod render;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use wayland_client::globals::registry_queue_init;
use wayland_client::Connection;

use crate::app::App;
use crate::config::Config;
use crate::ipc::{spawn_ipc_thread, Snapshot, UiMsg};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let config_path = Config::path();
    let config = Arc::new(RwLock::new(Config::load()?));
    let shared = Arc::new(RwLock::new(Snapshot::default()));

    let conn = Connection::connect_to_env()
        .context("failed to connect to the Wayland display (is WAYLAND_DISPLAY set?)")?;
    let (globals, queue) =
        registry_queue_init::<App>(&conn).context("failed to initialize Wayland globals")?;
    let qh = queue.handle();

    let mut app = App::new(&globals, &qh, shared.clone(), config.clone())?;
    app.init_outputs(&globals);

    let (tx, rx) = calloop::channel::channel::<UiMsg>();
    spawn_ipc_thread(shared, config.clone(), tx.clone())?;
    config::spawn_config_watcher(&config_path, tx.clone())?;

    let mut event_loop: calloop::EventLoop<App> = calloop::EventLoop::try_new()?;
    let handle = event_loop.handle();
    handle.insert_source(rx, |msg, _, app| {
        if let calloop::channel::Event::Msg(m) = msg {
            app.on_msg(m);
        }
    })
    .expect("failed to register the IPC channel");
    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .expect("failed to register the Wayland source");
    handle
        .insert_source(Timer::from_duration(Duration::from_millis(250)), |_, _, app| {
            app.on_hide_tick();
            TimeoutAction::ToDuration(Duration::from_millis(250))
        })
        .expect("failed to register the hide-tick timer");

    log::info!("nirimap started");
    event_loop.run(None, &mut app, |_| {})?;
    Ok(())
}
