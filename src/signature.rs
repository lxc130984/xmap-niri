//! Content signature for redraw skipping.
//!
//! Everything that can change the rendered pixels is folded into a 64-bit
//! FNV-1a digest. Events that leave the digest unchanged cannot change the
//! output, so the whole layout pass and redraw can be skipped.

use niri_ipc::state::EventStreamState;
use niri_ipc::{Window, Workspace};
use std::collections::HashMap;

use crate::config::Config;
use crate::layout;

/// Hash of everything that can change what the minimap draws.
#[allow(clippy::too_many_arguments)]
pub fn content_signature(
    cfg: &Config,
    surface_generation: u64,
    surface_scale: i32,
    configured: Option<(u32, u32)>,
    focused_output_name: Option<&str>,
    viewport_width: f32,
    output_height: f32,
    state: &EventStreamState,
    outputs: &HashMap<String, niri_ipc::Output>,
) -> u64 {
    let mut h = Fnv1a::new();
    let mode = cfg.display.mode.clone();

    h.hash(&mode);
    h.u64(cfg.display.height as u64);
    h.u64(cfg.display.max_width_percent.to_bits());
    h.u64(cfg.display.max_height_percent.to_bits());
    h.u64(cfg.display.follow_focus as u64);
    h.hash(&cfg.appearance.background);
    h.u64(cfg.appearance.background_opacity.to_bits());
    h.hash(&cfg.appearance.window_color);
    h.hash(&cfg.appearance.focused_color);
    h.hash(&cfg.appearance.border_color);
    h.u64(cfg.appearance.border_width.to_bits());
    h.u64(cfg.appearance.border_radius.to_bits());
    h.u64(cfg.appearance.gap.to_bits());
    h.u64(cfg.appearance.window_opacity.to_bits());
    h.u64(cfg.appearance.focused_opacity.to_bits());
    h.u64(cfg.appearance.show_icons as u64);
    h.u64(cfg.appearance.workspace_gap.to_bits());
    h.hash(&cfg.appearance.active_workspace_border_color);
    h.u64(cfg.appearance.active_workspace_border_width.to_bits());
    h.hash(&cfg.appearance.active_window_border_color);
    h.u64(cfg.appearance.active_window_border_width.to_bits());

    h.u64(surface_generation);
    h.u64(surface_scale as u64);
    if let Some((w, hh)) = configured {
        h.u64(w as u64);
        h.u64(hh as u64);
    }
    h.hash(focused_output_name.unwrap_or(""));
    h.u64(viewport_width.to_bits() as u64);
    h.u64(output_height.to_bits() as u64);

    // Output sizes feed the viewport-offset estimate for floating windows,
    // so a resolution change must invalidate the cached frame.
    let mut out_names: Vec<&String> = outputs.keys().collect();
    out_names.sort();
    for name in out_names {
        h.hash(name);
        if let Some(log) = outputs[name].logical {
            h.u64(log.width as u64);
            h.u64(log.height as u64);
        }
    }

    if mode == "all" {
        for ws in layout::all_rows(state) {
            hash_workspace(&mut h, ws);
        }
        let mut ids: Vec<u64> = state.windows.windows.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(w) = state.windows.windows.get(&id) {
                hash_window(&mut h, w);
            }
        }
    } else if let Some(ws) = layout::focused_workspace(state) {
        hash_workspace(&mut h, ws);
        let mut ids: Vec<u64> = state
            .windows
            .windows
            .iter()
            .filter(|(_, w)| w.workspace_id == Some(ws.id))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(w) = state.windows.windows.get(&id) {
                hash_window(&mut h, w);
            }
        }
    }

    h.finish()
}

/// 64-bit FNV-1a accumulator.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    fn hash(&mut self, s: &str) {
        for b in s.as_bytes() {
            self.byte(*b);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

fn hash_workspace(h: &mut Fnv1a, ws: &Workspace) {
    h.u64(ws.id);
    h.u64(ws.idx as u64);
    h.hash(ws.name.as_deref().unwrap_or(""));
    h.hash(ws.output.as_deref().unwrap_or(""));
    h.u64(ws.is_active as u64);
    h.u64(ws.is_focused as u64);
    h.u64(ws.active_window_id.unwrap_or(0));
}

fn hash_window(h: &mut Fnv1a, w: &Window) {
    h.u64(w.id);
    h.hash(w.app_id.as_deref().unwrap_or(""));
    h.u64(w.workspace_id.unwrap_or(0));
    h.u64(w.is_focused as u64);
    h.u64(w.is_floating as u64);
    h.u64(
        w.layout
            .pos_in_scrolling_layout
            .map(|p| p.0)
            .unwrap_or(0) as u64,
    );
    h.u64(
        w.layout
            .pos_in_scrolling_layout
            .map(|p| p.1)
            .unwrap_or(0) as u64,
    );
    h.u64(w.layout.tile_size.0.to_bits());
    h.u64(w.layout.tile_size.1.to_bits());
    h.u64(w.layout.window_size.0 as u64);
    h.u64(w.layout.window_size.1 as u64);
    h.u64(
        w.layout
            .tile_pos_in_workspace_view
            .map(|p| p.0.to_bits())
            .unwrap_or(0),
    );
    h.u64(
        w.layout
            .tile_pos_in_workspace_view
            .map(|p| p.1.to_bits())
            .unwrap_or(0),
    );
    h.u64(w.layout.window_offset_in_tile.0.to_bits());
    h.u64(w.layout.window_offset_in_tile.1.to_bits());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ipc::Snapshot;
    use niri_ipc::{Window, WindowLayout, Workspace};

    fn ws(id: u64, active: bool) -> Workspace {
        Workspace {
            id,
            idx: 0,
            name: Some(format!("{id}")),
            output: Some("eDP-1".into()),
            is_active: active,
            is_focused: active,
            is_urgent: false,
            active_window_id: None,
        }
    }

    fn win(id: u64, ws: u64, x: f64, y: f64) -> Window {
        Window {
            id,
            title: Some(format!("win{id}")),
            app_id: Some("test".into()),
            pid: None,
            workspace_id: Some(ws),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (800.0, 600.0),
                window_size: (800, 600),
                tile_pos_in_workspace_view: Some((x, y)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn unchanged_state_keeps_signature() {
        let cfg = Config::default();
        let mut snap = Snapshot::default();
        snap.state.workspaces.workspaces.insert(1, ws(1, true));
        snap.state.windows.windows.insert(10, win(10, 1, 0.0, 0.0));
        let args = (
            &cfg,
            1u64,
            1i32,
            Some((200u32, 100u32)),
            Some("eDP-1"),
            1920.0f32,
            1080.0f32,
            &snap.state,
        );
        let a = content_signature(
            args.0,
            args.1,
            args.2,
            args.3,
            args.4,
            args.5,
            args.6,
            args.7,
            &snap.outputs,
        );
        let b = content_signature(
            args.0,
            args.1,
            args.2,
            args.3,
            args.4,
            args.5,
            args.6,
            args.7,
            &snap.outputs,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn moving_a_window_changes_signature() {
        let cfg = Config::default();
        let mut snap = Snapshot::default();
        snap.state.workspaces.workspaces.insert(1, ws(1, true));
        snap.state.windows.windows.insert(10, win(10, 1, 0.0, 0.0));

        let base = content_signature(
            &cfg, 1, 1, Some((200, 100)), Some("eDP-1"), 1920.0, 1080.0, &snap.state, &snap.outputs,
        );
        snap.state
            .windows
            .windows
            .get_mut(&10)
            .unwrap()
            .layout
            .tile_pos_in_workspace_view = Some((400.0, 300.0));
        let moved = content_signature(
            &cfg, 1, 1, Some((200, 100)), Some("eDP-1"), 1920.0, 1080.0, &snap.state, &snap.outputs,
        );
        assert_ne!(base, moved);
    }
}
