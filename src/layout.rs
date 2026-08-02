//! Turn the niri IPC state into per-workspace minimap rows.

use std::collections::HashMap;

use niri_ipc::state::EventStreamState;
use niri_ipc::{Window, Workspace};

use crate::icons::{self, SharedIcons};

/// One tiled window tile in workspace coordinates (logical pixels).
#[derive(Debug, Clone)]
pub struct Tile {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub focused: bool,
    /// The workspace's last-focused (active) window.
    pub is_last_focused: bool,
    /// Wayland app_id used to resolve the desktop icon.
    pub app_id: Option<String>,
}

/// A workspace laid out in its own coordinate space.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub tiles: Vec<Tile>,
    /// Sum of column widths.
    pub total_width: f64,
    /// Tallest column height.
    pub max_height: f64,
}

impl Row {
    pub fn has_content(&self) -> bool {
        !self.tiles.is_empty()
    }
}

/// The globally focused workspace, if any.
pub fn focused_workspace(state: &EventStreamState) -> Option<&Workspace> {
    state.workspaces.workspaces.values().find(|w| w.is_focused)
}

/// Every workspace worth showing, sorted by (output, index).
///
/// Workspaces with windows plus the active/focused workspace are included, so
/// empty workspaces do not clutter the widget.
pub fn all_rows(state: &EventStreamState) -> Vec<&Workspace> {
    let mut list: Vec<&Workspace> = state.workspaces.workspaces.values().collect();
    list.sort_by(|a, b| {
        a.output
            .cmp(&b.output)
            .then_with(|| a.idx.cmp(&b.idx))
            .then_with(|| a.id.cmp(&b.id))
    });
    list.into_iter()
        .filter(|w| {
            w.is_active
                || w.is_focused
                || state
                    .windows
                    .windows
                    .values()
                    .any(|win| win.workspace_id == Some(w.id))
        })
        .collect()
}

/// Build the minimap row for one workspace (tiled windows only).
///
/// Niri's IPC exposes the column/tile position of tiled windows, so their
/// relative layout is fully known. Floating windows are excluded: niri only
/// reports their position while they happen to be inside the viewport, so they
/// cannot be placed reliably.
///
/// When `icons` is given (icon mode), windows whose app_id has no resolvable
/// icon are skipped, so the row geometry already reflects what will be drawn.
pub fn build_row(
    ws: &Workspace,
    windows: &HashMap<u64, Window>,
    icons: Option<&SharedIcons>,
) -> Row {
    struct Entry {
        id: u64,
        idx: usize,
        w: f64,
        h: f64,
        focused: bool,
        app_id: Option<String>,
    }

    let mut columns: std::collections::BTreeMap<usize, Vec<Entry>> =
        std::collections::BTreeMap::new();
    for win in windows.values() {
        if win.workspace_id != Some(ws.id) {
            continue;
        }
        if let Some(icons) = icons {
            if !win
                .app_id
                .as_deref()
                .is_some_and(|id| icons::has_icon(icons, id))
            {
                continue;
            }
        }
        let Some((col, tile)) = win.layout.pos_in_scrolling_layout else {
            continue; // floating or otherwise unpositioned
        };
        if col == 0 || tile == 0 {
            continue; // 1-based indices; ignore malformed values
        }
        columns.entry(col - 1).or_default().push(Entry {
            id: win.id,
            idx: tile - 1,
            w: win.layout.tile_size.0,
            h: win.layout.tile_size.1,
            focused: win.is_focused,
            app_id: win.app_id.clone(),
        });
    }

    let mut row = Row::default();
    let active = ws.active_window_id;
    let mut x = 0.0;

    for (col_idx, mut col) in columns.into_iter() {
        col.sort_by_key(|e| e.idx);
        let col_width = col.iter().map(|e| e.w).fold(0.0, f64::max);
        let mut y = 0.0;
        for e in &col {
            row.tiles.push(Tile {
                x,
                y,
                w: e.w,
                h: e.h,
                focused: e.focused,
                is_last_focused: active == Some(e.id) || (active.is_none() && e.focused),
                app_id: e.app_id.clone(),
            });
            y += e.h;
        }
        row.max_height = row.max_height.max(y);
        x += col_width;
        let _ = col_idx;
    }

    row.total_width = x;
    row
}
