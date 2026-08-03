mod app;
mod config;
mod icons;
mod ipc;
mod layout;
mod render;

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use calloop_wayland_source::WaylandSource;
use wayland_client::globals::registry_queue_init;
use wayland_client::Connection;

use crate::app::App;
use crate::config::Config;
use crate::icons::IconCache;
use crate::ipc::{spawn_ipc_thread, Snapshot, UiMsg};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = Config::path();
    let config = Arc::new(RwLock::new(Config::load()?));
    let shared = Arc::new(RwLock::new(Snapshot::default()));
    let icons = Arc::new(std::sync::Mutex::new(IconCache::default()));

    let conn = Connection::connect_to_env()
        .context("failed to connect to the Wayland display (is WAYLAND_DISPLAY set?)")?;
    let (globals, queue) =
        registry_queue_init::<App>(&conn).context("failed to initialize Wayland globals")?;
    let qh = queue.handle();
    log::info!(
        "connected to Wayland; found {} globals",
        globals.contents().clone_list().len()
    );

    let mut app = App::new(&globals, &qh, shared.clone(), config.clone(), icons.clone())?;
    app.init_outputs(&globals);

    let (tx, rx) = calloop::channel::channel::<UiMsg>();
    spawn_ipc_thread(shared, tx.clone(), icons)?;
    config::spawn_config_watcher(&config_path, tx.clone())?;

    let mut event_loop: calloop::EventLoop<App> = calloop::EventLoop::try_new()?;
    let handle = event_loop.handle();
    handle
        .insert_source(rx, |msg, _, app| {
            if let calloop::channel::Event::Msg(m) = msg {
                app.on_msg(m);
            }
        })
        .expect("failed to register the IPC channel");
    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .expect("failed to register the Wayland source");

    log::info!("nirimap started");
    event_loop.run(None, &mut app, |_| {})?;
    Ok(())
}
