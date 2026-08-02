//! The Wayland layer-shell overlay and the UI event loop state.
//!
//! Rendering is fully CPU-side: tiny-skia draws into a small shared-memory
//! buffer which is attached to a zwlr-layer-shell surface. Redraws are
//! event-driven and paced by the compositor's frame callbacks, so bursts of
//! IPC events never cost more than one repaint per display frame.

use std::fs::File;
use std::os::fd::{AsFd, FromRawFd};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use log::{debug, warn};
use memmap2::MmapMut;
use wayland_client::globals::{GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::{self, WlCompositor},
    wl_output::{self, WlOutput},
    wl_region::{self, WlRegion},
    wl_registry::{self, WlRegistry},
    wl_shm::{self, Format, WlShm},
    wl_shm_pool::{self, WlShmPool},
    wl_surface::{self, WlSurface},
};
use wayland_client::{delegate_noop, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::config::Config;
use crate::icons::SharedIcons;
use crate::ipc::{Shared, UiMsg};
use crate::{layout, render};

/// Maximum number of shm buffers kept around (double/triple buffering).
const MAX_BUFFERS: usize = 3;

pub struct App {
    qh: QueueHandle<App>,
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    outputs: Vec<OutputInfo>,
    overlay: Option<Overlay>,

    shared: Shared,
    config: Arc<RwLock<Config>>,
    icons: SharedIcons,

    buffers: Vec<ShmBuffer>,
    next_buffer_id: u64,

    visible: bool,
    dirty: bool,
    frame_cb: Option<WlCallback>,
    surface_scale: i32,
    /// Last size received from the compositor's configure event (logical px).
    configured: Option<(u32, u32)>,
    /// Last size we requested via set_size.
    desired_size: (u32, u32),
    /// A resize request is in flight; wait for the next configure.
    size_pending: bool,
    hide_deadline: Option<Instant>,

    focused_output_name: Option<String>,
    viewport_width: f32,
    output_height: f32,
}

struct OutputInfo {
    registry_name: u32,
    name: Option<String>,
    scale: i32,
    proxy: WlOutput,
}

struct Overlay {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    region: WlRegion,
    output: Option<WlOutput>,
}

struct ShmBuffer {
    id: u64,
    _file: File,
    mmap: MmapMut,
    pool: WlShmPool,
    buffer: WlBuffer,
    w: u32,
    h: u32,
    in_use: bool,
}

impl ShmBuffer {
    fn create(w: u32, h: u32, shm: &WlShm, qh: &QueueHandle<App>, id: u64) -> Result<Self> {
        let size = (w as usize) * (h as usize) * 4;
        let file = make_memfd()?;
        file.set_len(size as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file) }?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            (w * 4) as i32,
            Format::Argb8888,
            qh,
            (),
        );
        Ok(ShmBuffer {
            id,
            _file: file,
            mmap,
            pool,
            buffer,
            w,
            h,
            in_use: false,
        })
    }
}

fn make_memfd() -> Result<File> {
    let name = c"nirimap-shm";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl App {
    pub fn new(
        globals: &GlobalList,
        qh: &QueueHandle<App>,
        shared: Shared,
        config: Arc<RwLock<Config>>,
        icons: SharedIcons,
    ) -> Result<Self> {
        let compositor = globals.bind::<WlCompositor, _, _>(qh, 1..=4, ())?;
        let shm = globals.bind::<WlShm, _, _>(qh, 1..=1, ())?;
        let layer_shell = globals.bind::<ZwlrLayerShellV1, _, _>(qh, 1..=1, ())?;
        log::info!("bound wl_compositor, wl_shm and zwlr_layer_shell_v1");
        let visible = config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .behavior
            .always_visible;

        let mut app = App {
            qh: qh.clone(),
            compositor,
            shm,
            layer_shell,
            outputs: Vec::new(),
            overlay: None,
            shared,
            config,
            icons,
            buffers: Vec::new(),
            next_buffer_id: 1,
            visible,
            dirty: true,
            frame_cb: None,
            surface_scale: 1,
            configured: None,
            desired_size: (0, 0),
            size_pending: false,
            hide_deadline: None,
            focused_output_name: None,
            viewport_width: 1920.0,
            output_height: 1080.0,
        };
        app.recreate_overlay();
        Ok(app)
    }

    /// Bind the outputs announced during the initial registry roundtrip.
    pub fn init_outputs(&mut self, globals: &GlobalList) {
        globals.contents().with_list(|list| {
            for g in list {
                if g.interface == "wl_output" {
                    let proxy = globals.registry().bind::<WlOutput, _, _>(
                        g.name,
                        g.version.min(4),
                        &self.qh,
                        (),
                    );
                    self.outputs.push(OutputInfo {
                        registry_name: g.name,
                        name: None,
                        scale: 1,
                        proxy,
                    });
                }
            }
        });
        self.update_surface_scale();
    }

    pub fn on_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::StateChanged { show } => self.on_state_changed(show),
            UiMsg::ConfigReload => self.on_config_reload(),
        }
    }

    fn on_state_changed(&mut self, show: bool) {
        self.update_focused_output();
        self.update_viewport();

        let always = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .behavior
            .always_visible;
        if always {
            self.visible = true;
            self.hide_deadline = None;
            self.dirty = true;
        } else if show {
            self.visible = true;
            let timeout = self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .behavior
                .hide_timeout_ms
                .max(1);
            self.hide_deadline = Some(Instant::now() + Duration::from_millis(timeout));
            self.dirty = true;
        }
        if self.visible {
            self.update_desired_size();
        }
        self.pump();
    }

    fn on_config_reload(&mut self) {
        let Ok(new) = Config::load() else {
            return;
        };
        let old = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = new.clone();

        if new.display.anchor != old.display.anchor
            || new.display.margin_x != old.display.margin_x
            || new.display.margin_y != old.display.margin_y
        {
            self.recreate_overlay();
        }
        if new.behavior.always_visible {
            self.visible = true;
            self.hide_deadline = None;
            self.dirty = true;
        } else if old.behavior.always_visible {
            self.hide();
        }
        if self.visible {
            self.update_desired_size();
        }
        self.pump();
    }

    pub(crate) fn on_hide_tick(&mut self) {
        if let Some(deadline) = self.hide_deadline {
            if Instant::now() >= deadline {
                self.hide();
            }
        }
    }

    fn hide(&mut self) {
        self.hide_deadline = None;
        if !self.visible {
            return;
        }
        self.visible = false;
        self.dirty = false;
        if let Some(o) = &self.overlay {
            o.surface.attach(None, 0, 0);
            o.surface.commit();
        }
        self.free_buffers();
        debug!("minimap hidden");
    }

    /// Recreate the layer surface, e.g. when the focused output or the
    /// anchor/margins changed.
    fn recreate_overlay(&mut self) {
        if let Some(o) = self.overlay.take() {
            o.layer.destroy();
            o.surface.destroy();
            o.region.destroy();
        }
        self.frame_cb = None;
        self.configured = None;
        self.desired_size = (0, 0);
        self.size_pending = false;
        self.dirty = true;

        let surface = self.compositor.create_surface(&self.qh, ());
        let region = self.compositor.create_region(&self.qh, ());
        region.add(0, 0, 0, 0);

        let output = self.focused_output_proxy();
        log::info!(
            "creating layer surface (namespace=nirimap, output={})",
            output
                .as_ref()
                .map(|o| format!("{:?}", o.id()))
                .unwrap_or_else(|| "all".into())
        );
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            output.as_ref(),
            Layer::Overlay,
            "nirimap".to_string(),
            &self.qh,
            (),
        );
        {
            let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
            layer.set_anchor(anchor_bits(&cfg.display.anchor));
            layer.set_margin(
                cfg.display.margin_y,
                cfg.display.margin_x,
                cfg.display.margin_y,
                cfg.display.margin_x,
            );
        }
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.set_input_region(Some(&region));

        self.overlay = Some(Overlay {
            surface: surface.clone(),
            layer,
            region,
            output,
        });
        log::info!("layer surface created; requesting size");
        self.update_surface_scale();
        surface.set_buffer_scale(self.surface_scale.max(1));
        // First commit without a buffer: the compositor replies with a
        // configure event before we are allowed to attach one.
        self.update_desired_size();
    }

    fn update_focused_output(&mut self) {
        let name = {
            let shared = self.shared.read().unwrap_or_else(|e| e.into_inner());
            layout::focused_workspace(&shared.state).and_then(|w| w.output.clone())
        };
        if name != self.focused_output_name {
            debug!("focused output: {name:?}");
            self.focused_output_name = name;
            self.recreate_overlay();
        }
    }

    fn update_viewport(&mut self) {
        let shared = self.shared.read().unwrap_or_else(|e| e.into_inner());
        if let Some(name) = &self.focused_output_name {
            if let Some(o) = shared.outputs.get(name) {
                if let Some(log) = o.logical {
                    self.viewport_width = log.width as f32;
                    self.output_height = log.height as f32;
                }
            }
        }
    }

    fn focused_output_proxy(&self) -> Option<WlOutput> {
        let name = self.focused_output_name.as_deref();
        if let Some(n) = name {
            if let Some(o) = self.outputs.iter().find(|o| o.name.as_deref() == Some(n)) {
                return Some(o.proxy.clone());
            }
        }
        self.outputs.first().map(|o| o.proxy.clone())
    }

    fn update_surface_scale(&mut self) {
        let overlay_output = self
            .overlay
            .as_ref()
            .and_then(|o| o.output.as_ref())
            .map(|p| p.id());
        let scale = self
            .outputs
            .iter()
            .find(|o| {
                self.focused_output_name
                    .as_deref()
                    .is_some_and(|n| o.name.as_deref() == Some(n))
                    || (self.focused_output_name.is_none() && Some(o.proxy.id()) == overlay_output)
            })
            .map(|o| o.scale)
            .unwrap_or(1)
            .max(1);
        if scale != self.surface_scale {
            self.surface_scale = scale;
            if let Some(o) = &self.overlay {
                o.surface.set_buffer_scale(scale);
            }
            self.dirty = true;
        }
    }

    fn compute_desired_size(&self) -> (u32, u32) {
        let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
        let show_icons = cfg.appearance.show_icons;
        let height = cfg.display.height.clamp(8, 2048);
        let max_w = (self.viewport_width * cfg.display.max_width_percent as f32)
            .clamp(8.0, self.viewport_width)
            .max(8.0);
        let max_h = (self.output_height * cfg.display.max_height_percent as f32)
            .clamp(8.0, self.output_height)
            .max(8.0);
        let shared = self.shared.read().unwrap_or_else(|e| e.into_inner());

        if cfg.display.mode == "all" {
            let ws_list = layout::all_rows(&shared.state);
            let n = ws_list.len().max(1) as f32;
            let gap = cfg.appearance.workspace_gap as f32;
            let ideal_h = n * height as f32 + (n - 1.0) * gap + 2.0 * render::PADDING;
            let h = ideal_h
                .clamp(height as f32 + 2.0 * render::PADDING, max_h)
                .max(8.0);
            let row_h = (h - 2.0 * render::PADDING - (n - 1.0) * gap) / n;

            let mut global_max = 0.0_f32;
            let mut left = f32::INFINITY;
            let mut right = f32::NEG_INFINITY;
            for ws in &ws_list {
                let row = layout::build_row(
                    ws,
                    &shared.state.windows.windows,
                    show_icons.then_some(&self.icons),
                );
                if row.has_content() {
                    global_max = global_max.max(row.max_height as f32);
                    left = left.min(-row.align_x as f32);
                    right = right.max((row.total_width - row.align_x) as f32);
                }
            }
            let k = if global_max > 0.0 {
                row_h / global_max
            } else {
                0.0
            };
            let content_w = if left.is_finite() {
                (right - left) * k
            } else {
                0.0
            };
            let w = (content_w + 2.0 * render::PADDING)
                .max(height as f32)
                .min(max_w);
            (w as u32, h as u32)
        } else {
            let focused = layout::focused_workspace(&shared.state).map(|ws| {
                layout::build_row(
                    ws,
                    &shared.state.windows.windows,
                    show_icons.then_some(&self.icons),
                )
            });
            let mut w = height as f32;
            if let Some(row) = focused {
                if row.max_height > 0.0 {
                    let k = (height as f32 - 2.0 * render::PADDING) / row.max_height as f32;
                    w = (row.total_width as f32 * k + 2.0 * render::PADDING).max(height as f32);
                }
            }
            (w.min(max_w) as u32, height)
        }
    }

    fn update_desired_size(&mut self) {
        let Some(overlay) = self.overlay.as_ref() else {
            return;
        };
        let (w, h) = self.compute_desired_size();
        let (w, h) = (w.clamp(8, 4096), h.clamp(8, 4096));
        if (w, h) != self.desired_size {
            self.desired_size = (w, h);
            self.size_pending = true;
            overlay.layer.set_size(w, h);
            overlay.surface.commit();
        }
    }

    /// Redraw now if possible; otherwise wait for the next frame callback,
    /// configure event or buffer release.
    fn pump(&mut self) {
        if !self.visible || !self.dirty {
            return;
        }
        if self.frame_cb.is_some() || self.configured.is_none() || self.size_pending {
            return;
        }
        self.draw();
        self.dirty = false;
    }

    fn draw(&mut self) {
        let Some(surface) = self.overlay.as_ref().map(|o| o.surface.clone()) else {
            return;
        };
        let Some((log_w, log_h)) = self.configured else {
            return;
        };
        let scale = self.surface_scale.max(1);
        let phys_w = ((log_w as f32 * scale as f32).round() as u32).max(1);
        let phys_h = ((log_h as f32 * scale as f32).round() as u32).max(1);

        let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
        let mode = cfg.display.mode.clone();
        let appearance = cfg.appearance.clone();
        drop(cfg);

        let built: Vec<layout::Row>;
        let rows;
        let focused;
        let viewport_w = self.viewport_width;
        {
            let shared = self.shared.read().unwrap_or_else(|e| e.into_inner());
            if mode == "all" {
                let ws_list = layout::all_rows(&shared.state);
                built = ws_list
                    .iter()
                    .map(|ws| {
                        layout::build_row(
                            ws,
                            &shared.state.windows.windows,
                            appearance.show_icons.then_some(&self.icons),
                        )
                    })
                    .collect();
                rows = built
                    .iter()
                    .zip(ws_list.iter())
                    .map(|(row, ws)| render::RowView {
                        is_active: ws.is_active,
                        row,
                    })
                    .collect();
                focused = None;
            } else {
                rows = Vec::new();
                focused = layout::focused_workspace(&shared.state).map(|ws| {
                    layout::build_row(
                        ws,
                        &shared.state.windows.windows,
                        appearance.show_icons.then_some(&self.icons),
                    )
                });
            }
        }

        let Some(data) = render::render(
            phys_w,
            phys_h,
            scale as f32,
            &appearance,
            &mode,
            focused.as_ref(),
            &rows,
            viewport_w,
            &self.icons,
            appearance.show_icons,
        ) else {
            warn!("failed to allocate render pixmap");
            return;
        };

        if let Some(buf) = self.acquire_buffer(phys_w, phys_h) {
            log::debug!("attaching buffer {phys_w}x{phys_h}");
            let dst = &mut buf.mmap[..data.len()];
            dst.copy_from_slice(&data);
            let buffer = buf.buffer.clone();
            surface.attach(Some(&buffer), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, phys_w as i32, phys_h as i32);
            } else {
                surface.damage(0, 0, log_w as i32, log_h as i32);
            }
            let cb = surface.frame(&self.qh, ());
            self.frame_cb = Some(cb);
            surface.commit();
            log::debug!("buffer committed");
        } else {
            // All buffers are with the compositor; stay dirty and let a
            // release event trigger the redraw.
            self.dirty = true;
        }
    }

    fn acquire_buffer(&mut self, w: u32, h: u32) -> Option<&mut ShmBuffer> {
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| !b.in_use && b.w == w && b.h == h)
        {
            let b = &mut self.buffers[i];
            b.in_use = true;
            return Some(b);
        }
        if let Some(i) = self.buffers.iter().position(|b| !b.in_use) {
            let b = &mut self.buffers[i];
            b.buffer.destroy();
            b.pool.destroy();
            let id = b.id;
            match ShmBuffer::create(w, h, &self.shm, &self.qh, id) {
                Ok(nb) => {
                    *b = nb;
                    b.in_use = true;
                    Some(b)
                }
                Err(err) => {
                    warn!("failed to resize shm buffer: {err:#}");
                    None
                }
            }
        } else if self.buffers.len() < MAX_BUFFERS {
            let id = self.next_buffer_id;
            self.next_buffer_id += 1;
            match ShmBuffer::create(w, h, &self.shm, &self.qh, id) {
                Ok(mut nb) => {
                    nb.in_use = true;
                    self.buffers.push(nb);
                    self.buffers.last_mut()
                }
                Err(err) => {
                    warn!("failed to allocate shm buffer: {err:#}");
                    None
                }
            }
        } else {
            None
        }
    }

    fn free_buffers(&mut self) {
        for b in self.buffers.drain(..) {
            b.buffer.destroy();
            b.pool.destroy();
        }
    }

    fn on_configure(&mut self, w: u32, h: u32) {
        self.configured = Some((w.max(1), h.max(1)));
        self.size_pending = false;
        log::debug!("configure received: {w}x{h}");
        if self.visible {
            self.dirty = true;
        }
        self.pump();
    }

    fn on_frame_done(&mut self) {
        log::debug!("frame callback done");
        self.frame_cb = None;
        self.pump();
    }

    fn on_buffer_release(&mut self, proxy: &WlBuffer) {
        if let Some(b) = self
            .buffers
            .iter_mut()
            .find(|b| b.buffer.id() == proxy.id())
        {
            b.in_use = false;
        }
        if self.dirty {
            self.pump();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &wayland_client::Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    let proxy = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                    state.outputs.push(OutputInfo {
                        registry_name: name,
                        name: None,
                        scale: 1,
                        proxy,
                    });
                    state.update_surface_scale();
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.registry_name != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Scale { factor } => {
                if let Some(o) = state
                    .outputs
                    .iter_mut()
                    .find(|o| o.proxy.id() == proxy.id())
                {
                    o.scale = factor;
                }
                state.update_surface_scale();
                if state.visible {
                    state.dirty = true;
                    state.pump();
                }
            }
            wl_output::Event::Name { name } => {
                if let Some(o) = state
                    .outputs
                    .iter_mut()
                    .find(|o| o.proxy.id() == proxy.id())
                {
                    o.name = Some(name.clone());
                }
                let focused = state.focused_output_name.clone();
                state.update_surface_scale();
                if focused.as_deref() == Some(name.as_str()) {
                    // The IPC snapshot may have named the focused output
                    // before this wl_output.name event arrived.
                    state.recreate_overlay();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::Enter { output } = event {
            if let Some(o) = state.outputs.iter().find(|o| o.proxy.id() == output.id()) {
                if o.scale != state.surface_scale {
                    state.surface_scale = o.scale.max(1);
                    if let Some(ov) = &state.overlay {
                        ov.surface.set_buffer_scale(state.surface_scale);
                    }
                    state.dirty = true;
                    state.pump();
                }
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.on_buffer_release(proxy);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        state.on_frame_done();
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                state.on_configure(width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                warn!("layer surface closed by the compositor; recreating");
                state.recreate_overlay();
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

fn anchor_bits(anchor: &str) -> Anchor {
    use Anchor as A;
    match anchor.to_ascii_lowercase().replace('_', "-").as_str() {
        "top-left" => A::Top | A::Left,
        "top-center" | "top" => A::Top,
        "top-right" => A::Top | A::Right,
        "bottom-left" => A::Bottom | A::Left,
        "bottom-center" | "bottom" => A::Bottom,
        "bottom-right" => A::Bottom | A::Right,
        "left" => A::Left,
        "right" => A::Right,
        "center" | "center-center" => A::empty(),
        _ => A::Top | A::Right,
    }
}
