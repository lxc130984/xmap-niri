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
    /// Sum of tiled column widths (floating windows excluded).
    pub total_width: f64,
    /// Tallest tiled column height (floating windows excluded).
    pub max_height: f64,
    /// Horizontal extent (right edge) of floating windows.
    pub float_width: f64,
    /// Vertical extent (bottom edge) of floating windows.
    pub float_height: f64,
}

impl Row {
    pub fn has_content(&self) -> bool {
        !self.tiles.is_empty()
    }

    /// Whether the row has any tiled windows (the scaling baseline).
    pub fn has_tiled(&self) -> bool {
        self.total_width > 0.0 && self.max_height > 0.0
    }

    /// Width metric used for scaling: tiled content when present, otherwise
    /// the floating extent. Floating windows never change a tiled row's
    /// scale, so dragging one does not resize the minimap.
    pub fn scale_w(&self) -> f64 {
        if self.has_tiled() {
            self.total_width
        } else {
            self.float_width
        }
    }

    /// Height metric used for scaling; see [`Row::scale_w`].
    pub fn scale_h(&self) -> f64 {
        if self.has_tiled() {
            self.max_height
        } else {
            self.float_height
        }
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

/// Build the minimap row for one workspace.
///
/// Niri's IPC exposes the column/tile position of tiled windows, so their
/// relative layout is fully known. Floating windows are placed from their
/// viewport-relative position, shifted into workspace coordinates by the
/// row's scroll offset; they are skipped while outside the viewport, where
/// niri stops reporting a position.
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
        /// The tile's x position within the workspace viewport.
        view_x: Option<f64>,
    }
    struct Float {
        id: u64,
        vx: f64,
        vy: f64,
        w: f64,
        h: f64,
        focused: bool,
        app_id: Option<String>,
    }

    let mut columns: std::collections::BTreeMap<usize, Vec<Entry>> =
        std::collections::BTreeMap::new();
    let mut floats: Vec<Float> = Vec::new();
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
        match win.layout.pos_in_scrolling_layout {
            Some((col, tile)) => {
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
                    view_x: win.layout.tile_pos_in_workspace_view.map(|p| p.0),
                });
            }
            None => {
                // Floating window: niri reports its position relative to the
                // workspace viewport, and only while it is inside it.
                let Some((vx, vy)) = win.layout.tile_pos_in_workspace_view else {
                    continue;
                };
                let (w, h) = (
                    win.layout.window_size.0 as f64,
                    win.layout.window_size.1 as f64,
                );
                if w <= 0.0 || h <= 0.0 {
                    continue;
                }
                floats.push(Float {
                    id: win.id,
                    vx,
                    vy,
                    w,
                    h,
                    focused: win.is_focused,
                    app_id: win.app_id.clone(),
                });
            }
        }
    }

    let mut row = Row::default();
    let active = ws.active_window_id;
    // Workspace-x of the viewport's left edge, derived from any tiled window
    // that reports its viewport position. Needed to place floating windows in
    // the same coordinate space as the tiled layout.
    let mut align_x: Option<f64> = None;
    let mut x = 0.0;

    for mut col in columns.into_values() {
        col.sort_by_key(|e| e.idx);
        let col_width = col.iter().map(|e| e.w).fold(0.0, f64::max);
        let mut y = 0.0;
        for e in &col {
            if align_x.is_none() {
                if let Some(vx) = e.view_x {
                    align_x = Some(x - vx);
                }
            }
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
    }

    row.total_width = x;

    // Floating windows sit on top of the tiled layout at their viewport
    // position, shifted into workspace coordinates by the scroll offset.
    let align_x = align_x.unwrap_or(0.0);
    for f in floats {
        let fx = align_x + f.vx;
        row.tiles.push(Tile {
            x: fx,
            y: f.vy,
            w: f.w,
            h: f.h,
            focused: f.focused,
            is_last_focused: active == Some(f.id) || (active.is_none() && f.focused),
            app_id: f.app_id,
        });
        row.float_height = row.float_height.max(f.vy + f.h);
        row.float_width = row.float_width.max(fx + f.w);
    }

    row
}
