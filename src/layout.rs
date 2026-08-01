//! Turn the niri IPC state into per-workspace minimap rows.

use std::collections::HashMap;

use niri_ipc::state::EventStreamState;
use niri_ipc::{Window, Workspace};

/// One tiled window tile in workspace coordinates (logical pixels).
#[derive(Debug, Clone)]
pub struct Tile {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub focused: bool,
}

/// A workspace laid out in its own coordinate space.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub tiles: Vec<Tile>,
    /// Sum of column widths.
    pub total_width: f64,
    /// Tallest column height.
    pub max_height: f64,
    /// Viewport offset: workspace-x of the viewport's left edge.
    pub align_x: f64,
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
pub fn build_row(ws: &Workspace, windows: &HashMap<u64, Window>) -> Row {
    struct Entry {
        idx: usize,
        w: f64,
        h: f64,
        view_pos: Option<(f64, f64)>,
        focused: bool,
    }

    let mut columns: std::collections::BTreeMap<usize, Vec<Entry>> =
        std::collections::BTreeMap::new();
    for win in windows.values() {
        if win.workspace_id != Some(ws.id) {
            continue;
        }
        let Some((col, tile)) = win.layout.pos_in_scrolling_layout else {
            continue; // floating or otherwise unpositioned
        };
        if col == 0 || tile == 0 {
            continue; // 1-based indices; ignore malformed values
        }
        columns.entry(col - 1).or_default().push(Entry {
            idx: tile - 1,
            w: win.layout.tile_size.0,
            h: win.layout.tile_size.1,
            view_pos: win.layout.tile_pos_in_workspace_view,
            focused: win.is_focused,
        });
    }

    let mut row = Row::default();
    let mut align_x: Option<f64> = None;
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
            });
            // This window's tile is at viewport-x `e.view_pos.0`, while its
            // workspace-x is the current column's start. The difference is the
            // viewport offset. (col_idx unused; x already holds the offset.)
            if let Some((px, _)) = e.view_pos {
                align_x.get_or_insert(x - px);
            }
            y += e.h;
        }
        row.max_height = row.max_height.max(y);
        x += col_width;
        let _ = col_idx;
    }

    row.total_width = x;
    row.align_x = align_x.unwrap_or(0.0);
    row
}
