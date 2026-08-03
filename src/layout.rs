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
    /// Viewport left edge (workspace x) this row was laid out with, when it
    /// could be derived from window data. `None` means the layout reused the
    /// caller's cached offset for this workspace.
    pub view_left: Option<f64>,
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

/// Logical width of the output a workspace is shown on.
///
/// This is the width of niri's "workspace view" for that output, used to
/// estimate the viewport offset for floating windows.
pub fn workspace_viewport_width(
    outputs: &HashMap<String, niri_ipc::Output>,
    ws: &Workspace,
) -> Option<f64> {
    let name = ws.output.as_deref()?;
    let output = outputs.get(name)?;
    Some(output.logical?.width as f64)
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
/// viewport's scroll offset; they are skipped while outside the viewport,
/// where niri stops reporting a position.
///
/// The scroll offset itself is taken from any tiled window that reports its
/// viewport position. Niri currently only reports that for floating windows
/// (see YaLTeR/niri#2381), so when it is missing we estimate the offset from
/// the anchor column — the focused column, or the workspace's last-focused
/// tiled column — by assuming niri centers it in a viewport of
/// `viewport_w` logical pixels. The estimate is clamped to the workspace
/// bounds, mirroring niri's own view limits.
///
/// Niri never scrolls the view when a floating window is focused (the
/// floating layout is a screen-fixed overlay), so when no tiled column can
/// anchor the estimate the caller's `prev_view_left` — the last known offset
/// for this workspace — is reused; `Row::view_left` then reports `None` so
/// the caller does not overwrite its cache with a guess.
///
/// When `icons` is given (icon mode), windows whose app_id has no resolvable
/// icon are skipped, so the row geometry already reflects what will be drawn.
pub fn build_row(
    ws: &Workspace,
    windows: &HashMap<u64, Window>,
    viewport_w: Option<f64>,
    prev_view_left: Option<f64>,
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

    // Anchor column for estimating the viewport offset: the focused column
    // when focus is tiled, otherwise the workspace's last-focused tiled
    // column. Only columns that survive icon filtering are eligible.
    let focused_tiled_id = windows.values().find(|w| {
        w.workspace_id == Some(ws.id)
            && w.is_focused
            && w.layout.pos_in_scrolling_layout.is_some()
    });
    let active_tiled_id = ws.active_window_id.and_then(|id| windows.get(&id)).filter(|w| {
        w.workspace_id == Some(ws.id) && w.layout.pos_in_scrolling_layout.is_some()
    });
    let anchor_id = focused_tiled_id
        .or(active_tiled_id)
        .map(|w| w.id);
    let mut anchor_col: Option<usize> = None;

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
                let col_idx = col - 1;
                if anchor_id == Some(win.id) {
                    anchor_col = Some(col_idx);
                }
                columns.entry(col_idx).or_default().push(Entry {
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
    let mut anchor_col_x: Option<f64> = None;
    let mut anchor_col_w: f64 = 0.0;
    let mut x = 0.0;

    for (col_idx, mut col) in columns.into_iter() {
        col.sort_by_key(|e| e.idx);
        let col_width = col.iter().map(|e| e.w).fold(0.0, f64::max);
        if Some(col_idx) == anchor_col {
            anchor_col_x = Some(x);
            anchor_col_w = col_width;
        }
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
    // The exact offset comes from any tiled window that reports its
    // viewport position; otherwise we estimate it from the anchor column
    // (see the function docs), clamped like niri's own view.
    let derived = align_x.or_else(|| {
        let (cx, cw) = (anchor_col_x?, anchor_col_w);
        let vw = viewport_w?;
        if vw <= 0.0 || row.total_width <= 0.0 {
            return Some(0.0);
        }
        let max_left = (row.total_width - vw).max(0.0);
        Some((cx + cw / 2.0 - vw / 2.0).clamp(0.0, max_left))
    });
    let view_left = derived.or(prev_view_left).unwrap_or(0.0);
    row.view_left = derived;
    for f in floats {
        let fx = view_left + f.vx;
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

#[cfg(test)]
mod tests {
    use super::*;
    use niri_ipc::WindowLayout;

    fn ws(id: u64, active_window_id: Option<u64>) -> Workspace {
        Workspace {
            id,
            idx: 0,
            name: Some(format!("{id}")),
            output: Some("eDP-1".into()),
            is_active: true,
            is_focused: true,
            is_urgent: false,
            active_window_id,
        }
    }

    fn tiled(id: u64, ws: u64, col: usize, w: f64, h: f64, focused: bool) -> Window {
        Window {
            id,
            title: None,
            app_id: Some("test".into()),
            pid: None,
            workspace_id: Some(ws),
            is_focused: focused,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                // Like real niri 25.08+: tiled windows report no viewport
                // position, so the layout has to estimate the view offset.
                pos_in_scrolling_layout: Some((col, 1)),
                tile_size: (w, h),
                window_size: (w as i32, h as i32),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn floating(id: u64, ws: u64, vx: f64, vy: f64, w: f64, h: f64) -> Window {
        Window {
            id,
            title: None,
            app_id: Some("test".into()),
            pid: None,
            workspace_id: Some(ws),
            is_focused: false,
            is_floating: true,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (w, h),
                window_size: (w as i32, h as i32),
                tile_pos_in_workspace_view: Some((vx, vy)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn windows_of(items: Vec<Window>) -> HashMap<u64, Window> {
        items.into_iter().map(|w| (w.id, w)).collect()
    }

    fn float_of(row: &Row, w: f64) -> &Tile {
        row.tiles.iter().find(|t| t.w == w).unwrap()
    }

    #[test]
    fn floats_follow_the_focused_column() {
        // Four 800px columns = 3200px workspace, 1920px viewport, one float.
        let mut windows = windows_of(vec![
            tiled(10, 1, 1, 800.0, 600.0, false),
            tiled(11, 1, 2, 800.0, 600.0, false),
            tiled(12, 1, 3, 800.0, 600.0, false),
            tiled(13, 1, 4, 800.0, 600.0, false),
            floating(20, 1, 100.0, 200.0, 300.0, 200.0),
        ]);
        let workspace = ws(1, None);

        // Focus column 2: viewport left = 1200 - 960 = 240 (inside bounds),
        // so the float lands at 240 + 100.
        windows.get_mut(&11).unwrap().is_focused = true;
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(row.view_left, Some(240.0));
        assert_eq!(
            float_of(&row, 300.0).x,
            340.0,
            "float = viewport left (240) + view x (100)"
        );

        // Focus column 1: the view clamps at the workspace start.
        windows.get_mut(&11).unwrap().is_focused = false;
        windows.get_mut(&10).unwrap().is_focused = true;
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(float_of(&row, 300.0).x, 100.0, "viewport left clamped to 0");

        // Focus column 4: the view clamps at 3200 - 1920 = 1280.
        windows.get_mut(&10).unwrap().is_focused = false;
        windows.get_mut(&13).unwrap().is_focused = true;
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(
            float_of(&row, 300.0).x,
            1380.0,
            "viewport left clamped to 1280"
        );
    }

    #[test]
    fn floats_anchor_to_last_focused_tiled_column() {
        // Focus lives in another workspace; the workspace's active
        // (last-focused) tiled window anchors the view estimate.
        let windows = windows_of(vec![
            tiled(10, 1, 1, 800.0, 600.0, false),
            tiled(11, 1, 2, 800.0, 600.0, false),
            tiled(12, 1, 3, 800.0, 600.0, false),
            tiled(13, 1, 4, 800.0, 600.0, false),
            floating(20, 1, 100.0, 200.0, 300.0, 200.0),
        ]);
        let workspace = ws(1, Some(11));
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(
            float_of(&row, 300.0).x,
            340.0,
            "anchored to last-focused column 2"
        );
    }

    #[test]
    fn float_falls_back_to_view_x_without_anchor() {
        // No tiled window and no viewport width: floats keep their raw
        // viewport-relative position.
        let windows = windows_of(vec![floating(20, 1, 100.0, 200.0, 300.0, 200.0)]);
        let workspace = ws(1, Some(20));
        let row = build_row(&workspace, &windows, None, None, None);
        assert_eq!(float_of(&row, 300.0).x, 100.0);
    }

    #[test]
    fn exact_view_offset_wins_over_estimate() {
        // When a tiled window does report its viewport position (niri >=
        // 25.08 reports it for floats, and older/newer builds may do so for
        // tiled windows), that exact offset takes precedence over the
        // focus-column estimate.
        let mut windows = windows_of(vec![
            tiled(10, 1, 1, 800.0, 600.0, true),
            floating(20, 1, 100.0, 200.0, 300.0, 200.0),
        ]);
        windows.get_mut(&10).unwrap().layout.tile_pos_in_workspace_view =
            Some((-200.0, 0.0)); // viewport left edge is at workspace x 200
        let workspace = ws(1, Some(10));
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(float_of(&row, 300.0).x, 300.0, "200 (exact) + 100 (view x)");
    }

    #[test]
    fn float_focus_reuses_cached_view_offset() {
        // Four 800px columns = 3200px workspace, 1920px viewport, one float.
        let mut windows = windows_of(vec![
            tiled(10, 1, 1, 800.0, 600.0, false),
            tiled(11, 1, 2, 800.0, 600.0, false),
            tiled(12, 1, 3, 800.0, 600.0, false),
            tiled(13, 1, 4, 800.0, 600.0, false),
            floating(20, 1, 100.0, 200.0, 300.0, 200.0),
        ]);
        let workspace = ws(1, None);

        // Tiled focus on column 2 derives offset 240.
        windows.get_mut(&11).unwrap().is_focused = true;
        let row = build_row(&workspace, &windows, Some(1920.0), None, None);
        assert_eq!(row.view_left, Some(240.0));
        assert_eq!(float_of(&row, 300.0).x, 340.0);

        // Focus moves to the floating window. Niri never scrolls the view
        // for a float, so the previously derived offset is reused and the
        // float stays exactly where it was.
        windows.get_mut(&11).unwrap().is_focused = false;
        windows.get_mut(&20).unwrap().is_focused = true;
        let row = build_row(&workspace, &windows, Some(1920.0), Some(240.0), None);
        assert_eq!(row.view_left, None, "fallback must not overwrite the cache");
        assert_eq!(float_of(&row, 300.0).x, 340.0, "float keeps its position");
    }
}
