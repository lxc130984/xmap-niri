//! CPU rendering of the minimap into a premultiplied BGRA8888 buffer
//! (wl_shm ARGB8888 byte order).

use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::config::{parse_hex, AppearanceConfig};
use crate::icons::{self, draw_icon, SharedIcons};
use crate::layout::Row;

/// Padding around the content inside the widget, logical pixels.
pub const PADDING: f32 = 4.0;

/// One workspace row to draw in "all" mode.
pub struct RowView {
    pub is_active: bool,
    pub row: Row,
}

/// Render the minimap into a reusable scratch buffer.
///
/// `phys_w`/`phys_h` are buffer pixels, `scale` is the integer buffer scale.
/// The buffer is resized in place and returned in `scratch`, so steady-state
/// redraws never allocate a fresh pixmap.
#[allow(clippy::too_many_arguments)]
pub fn render_into(
    scratch: &mut Vec<u8>,
    phys_w: u32,
    phys_h: u32,
    scale: f32,
    cfg: &AppearanceConfig,
    mode: &str,
    focused: Option<&Row>,
    rows: &[RowView],
    icons: &SharedIcons,
    show_icons: bool,
) -> Option<()> {
    let len = phys_w
        .checked_mul(phys_h)
        .and_then(|n| n.checked_mul(4))? as usize;
    scratch.resize(len, 0);
    // `Vec::resize` keeps the previous bytes; `Pixmap::from_vec` does not
    // zero them either. Regions the painter does not touch (transparent
    // background, moving tiles) would otherwise keep last frame's pixels and
    // ghost into the new frame, which looks like corrupted output whenever
    // content moves — floating windows are the most visible case.
    scratch.fill(0);
    let mut pixmap =
        Pixmap::from_vec(std::mem::take(scratch), tiny_skia::IntSize::from_wh(phys_w, phys_h)?)?;
    let s = Transform::from_scale(scale, scale);
    let log_w = phys_w as f32 / scale;
    let log_h = phys_h as f32 / scale;

    if cfg.background_opacity > 0.0 {
        if let Some((r, g, b)) = parse_hex(&cfg.background) {
            let mut paint = Paint::default();
            set_paint_color(&mut paint, r, g, b, cfg.background_opacity as f32);
            let path = rounded_rect(
                Rect::from_xywh(0.0, 0.0, log_w, log_h)?,
                cfg.border_radius as f32,
            );
            pixmap.fill_path(&path, &paint, FillRule::Winding, s, None);
        }
    }

    if mode == "all" {
        draw_all(&mut pixmap, s, cfg, rows, log_w, log_h, icons, show_icons);
    } else {
        draw_current(
            &mut pixmap,
            s,
            cfg,
            focused,
            log_w,
            log_h,
            icons,
            show_icons,
        );
    }

    // tiny-skia is RGBA; wl_shm ARGB8888 wants BGRA.
    let mut data = pixmap.take();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    *scratch = data;
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn draw_current(
    pix: &mut Pixmap,
    s: Transform,
    cfg: &AppearanceConfig,
    focused: Option<&Row>,
    log_w: f32,
    log_h: f32,
    icons: &SharedIcons,
    show_icons: bool,
) {
    let Some(row) = focused else {
        return;
    };
    if !row.has_content() {
        return;
    }
    let inner_h = (log_h - 2.0 * PADDING).max(0.0);
    if inner_h <= 0.0 || row.scale_h() <= 0.0 {
        return;
    }
    let k = inner_h / row.scale_h() as f32;
    let content_w = row.scale_w() as f32 * k;
    let x0 = PADDING + ((log_w - 2.0 * PADDING - content_w).max(0.0)) / 2.0;
    let y0 = PADDING;
    for t in &row.tiles {
        draw_tile(
            pix,
            s,
            cfg,
            x0 + t.x as f32 * k,
            y0 + t.y as f32 * k,
            t.w as f32 * k,
            t.h as f32 * k,
            t.focused,
            t.is_last_focused,
            None,
            icons,
            t.app_id.as_deref(),
            show_icons,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_all(
    pix: &mut Pixmap,
    s: Transform,
    cfg: &AppearanceConfig,
    rows: &[RowView],
    log_w: f32,
    log_h: f32,
    icons: &SharedIcons,
    show_icons: bool,
) {
    if rows.is_empty() {
        return;
    }
    let n = rows.len() as f32;
    let gap = cfg.workspace_gap as f32;
    let row_h = ((log_h - 2.0 * PADDING - (n - 1.0) * gap) / n).max(1.0);

    // One shared scale for all tiled rows, derived from the tallest tiled
    // column, so rows stay comparable. Floating windows are overlays and are
    // deliberately excluded: dragging one must not resize every row.
    let max_tiled_h = rows
        .iter()
        .filter(|r| r.row.has_tiled())
        .map(|r| r.row.max_height as f32)
        .fold(0.0_f32, f32::max);
    if max_tiled_h <= 0.0 && rows.iter().all(|r| !r.row.has_content()) {
        return;
    }
    let k = if max_tiled_h > 0.0 {
        row_h / max_tiled_h
    } else {
        0.0
    };

    let inner_w = (log_w - 2.0 * PADDING).max(0.0);

    let mut y = PADDING;
    for r in rows {
        // Rows without tiled windows scale by their own floating extent so
        // their windows stay visible; everything else shares the global k.
        let (rk, content_w) = if r.row.has_tiled() {
            (k, r.row.total_width as f32 * k)
        } else if r.row.float_height > 0.0 {
            let rk = row_h / r.row.float_height as f32;
            (rk, r.row.float_width as f32 * rk)
        } else {
            (0.0, 0.0)
        };
        // Every row is left-aligned: the same workspace coordinate always
        // maps to the same widget coordinate, so rows never shift relative
        // to each other.
        let row_w = content_w.min(inner_w).max(1.0);
        if r.is_active && cfg.active_workspace_border_width > 0.0 {
            let mut paint = Paint::default();
            match parse_hex(&cfg.active_workspace_border_color) {
                Some((cr, cg, cb)) => set_paint_color(&mut paint, cr, cg, cb, 1.0),
                None => set_paint_color(&mut paint, 0.54, 0.71, 0.98, 1.0),
            }
            // The highlight hugs this row's content so it can never drift
            // away from the tiles it marks.
            let rect = rect_min(PADDING, y, row_w, row_h);
            let path = rounded_rect(rect, cfg.border_radius as f32);
            pix.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width: cfg.active_workspace_border_width as f32,
                    ..Stroke::default()
                },
                s,
                None,
            );
        }

        if r.row.has_content() {
            let clip = rect_min(PADDING, y, inner_w, row_h);
            for t in &r.row.tiles {
                let x = PADDING + t.x as f32 * rk;
                let ty = y + t.y as f32 * rk;
                draw_tile(
                    pix,
                    s,
                    cfg,
                    x,
                    ty,
                    t.w as f32 * rk,
                    t.h as f32 * rk,
                    t.focused,
                    t.is_last_focused,
                    Some(clip),
                    icons,
                    t.app_id.as_deref(),
                    show_icons,
                );
            }
        }
        y += row_h + gap;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tile(
    pix: &mut Pixmap,
    s: Transform,
    cfg: &AppearanceConfig,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    focused: bool,
    is_last_focused: bool,
    clip: Option<Rect>,
    icons: &SharedIcons,
    app_id: Option<&str>,
    show_icons: bool,
) {
    let gap = (cfg.gap as f32) * 0.5;
    let (mut x, mut y, mut w, mut h) = (
        x + gap,
        y + gap,
        (w - gap * 2.0).max(1.0),
        (h - gap * 2.0).max(1.0),
    );
    if let Some(c) = clip {
        // Cheap rect clipping instead of a full mask: the "clip" is always a
        // plain rectangle here, so intersect before rasterizing.
        let nx = x.max(c.left());
        let ny = y.max(c.top());
        let nw = (x + w).min(c.right()) - nx;
        let nh = (y + h).min(c.bottom()) - ny;
        if nw <= 0.0 || nh <= 0.0 {
            return;
        }
        x = nx;
        y = ny;
        w = nw;
        h = nh;
    }
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let path = rounded_rect(rect, cfg.border_radius as f32);

    let (fill, alpha) = if focused {
        (cfg.focused_color.clone(), cfg.focused_opacity as f32)
    } else {
        (cfg.window_color.clone(), cfg.window_opacity as f32)
    };
    if alpha > 0.0 {
        if let Some((r, g, b)) = parse_hex(&fill) {
            let mut paint = Paint::default();
            set_paint_color(&mut paint, r, g, b, alpha);
            pix.fill_path(&path, &paint, FillRule::Winding, s, None);
        }
    }

    // The workspace's last-focused window gets a special border so its
    // position is easy to spot in the "all" preview.
    let (border_color, border_width) = if is_last_focused {
        (
            &cfg.active_window_border_color,
            cfg.active_window_border_width,
        )
    } else {
        (&cfg.border_color, cfg.border_width)
    };
    if border_width > 0.0 {
        if let Some((r, g, b)) = parse_hex(border_color) {
            let mut paint = Paint::default();
            set_paint_color(&mut paint, r, g, b, 1.0);
            pix.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width: border_width as f32,
                    ..Stroke::default()
                },
                s,
                None,
            );
        }
    }

    if show_icons {
        if let Some(app_id) = app_id {
            icons::with_icon(icons, app_id, |icon| {
                if let Some(icon) = icon {
                    draw_icon(pix, icon, scale_of(s), x, y, w, h);
                }
            });
        }
    }
}

/// Uniform scale factor of the widget's buffer transform.
fn scale_of(s: Transform) -> f32 {
    // The transform is always `from_scale(n, n)`; fall back to 1.0 defensively.
    let (_, sy) = s.get_scale();
    if sy > 0.0 {
        sy
    } else {
        1.0
    }
}

fn set_paint_color(paint: &mut Paint, r: f32, g: f32, b: f32, a: f32) {
    let color = Color::from_rgba(r, g, b, a.clamp(0.0, 1.0)).unwrap_or(Color::BLACK);
    paint.set_color(color);
}

/// A rect with a minimum 1px size; tiny-skia rejects degenerate/empty rects
/// in some paths, and our callers only need it for clamping/stroking.
fn rect_min(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)).unwrap()
}

fn rounded_rect(rect: Rect, radius: f32) -> Path {
    let r = radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0);
    let (x, y, w, h) = (rect.left(), rect.top(), rect.width(), rect.height());
    let mut pb = PathBuilder::new();
    if r <= 0.001 {
        pb.push_rect(rect);
    } else {
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
    }
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(rect))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppearanceConfig, Config};
    use crate::icons::IconCache;
    use crate::ipc::Snapshot;
    use crate::layout;
    use niri_ipc::{Window, WindowLayout, Workspace};
    use std::sync::{Arc, Mutex};

    fn test_window(
        id: u64,
        ws: u64,
        col: usize,
        tile: usize,
        w: f64,
        h: f64,
        view_x: f64,
    ) -> Window {
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
                pos_in_scrolling_layout: Some((col, tile)),
                tile_size: (w, h),
                window_size: (w as i32, h as i32),
                tile_pos_in_workspace_view: Some((view_x, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn ws(id: u64, active: bool, focused: bool) -> Workspace {
        Workspace {
            id,
            idx: 0,
            name: Some(format!("{id}")),
            output: Some("eDP-1".into()),
            is_active: active,
            is_focused: focused,
            is_urgent: false,
            active_window_id: None,
        }
    }

    #[test]
    fn renders_pixels_for_tiled_windows() {
        let mut snap = Snapshot::default();
        snap.state
            .workspaces
            .workspaces
            .insert(1, ws(1, true, true));
        snap.state
            .windows
            .windows
            .insert(10, test_window(10, 1, 1, 1, 800.0, 600.0, 0.0));
        snap.state
            .windows
            .windows
            .insert(11, test_window(11, 1, 1, 2, 800.0, 400.0, 0.0));

        let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
        let row = layout::build_row(ws, &snap.state.windows.windows, None, None, None);
        assert!(row.has_content());
        assert_eq!(row.tiles.len(), 2);

        let cfg = Config::default();
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let mut scratch = Vec::new();
        render_into(
            &mut scratch,
            200,
            200,
            1.0,
            &cfg.appearance,
            "current",
            Some(&row),
            &[],
            &icons,
            false,
        )
        .expect("render should succeed");
        let data = &scratch;
        assert_eq!(data.len(), 200 * 200 * 4);
        // With show_icons=false, tiles draw colored rects: at least one pixel
        // must be non-transparent.
        assert!(data.chunks_exact(4).any(|px| px[3] != 0));

        // Icon mode with an unresolvable app_id filters the row to empty.
        let row = layout::build_row(ws, &snap.state.windows.windows, None, None, Some(&icons));
        assert!(!row.has_content());
    }

    #[test]
    fn appearance_defaults_are_sane() {
        let a = AppearanceConfig::default();
        assert!(a.show_icons);
        assert_eq!(
            parse_hex(&a.window_color),
            Some((69.0 / 255.0, 71.0 / 255.0, 90.0 / 255.0))
        );
    }

    #[test]
    fn all_mode_rows_are_left_aligned() {
        // Two workspaces with different scroll offsets: the same workspace x
        // coordinate must map to the same widget x in every row, and the
        // active-workspace highlight must hug the row's content width.
        let mut snap = Snapshot::default();
        snap.state
            .workspaces
            .workspaces
            .insert(1, ws(1, true, true));
        snap.state
            .workspaces
            .workspaces
            .insert(2, ws(2, false, false));
        snap.state
            .windows
            .windows
            .insert(10, test_window(10, 1, 1, 1, 400.0, 600.0, 50.0));
        snap.state
            .windows
            .windows
            .insert(12, test_window(12, 2, 1, 1, 400.0, 800.0, 200.0));

        let ws1 = snap.state.workspaces.workspaces.get(&1).unwrap();
        let ws2 = snap.state.workspaces.workspaces.get(&2).unwrap();
        let row1 = layout::build_row(ws1, &snap.state.windows.windows, None, None, None);
        let row2 = layout::build_row(ws2, &snap.state.windows.windows, None, None, None);
        let rows = [
            RowView {
                is_active: true,
                row: row1,
            },
            RowView {
                is_active: false,
                row: row2,
            },
        ];

        let cfg = Config::default();
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let mut scratch = Vec::new();
        render_into(
            &mut scratch,
            128,
            212,
            1.0,
            &cfg.appearance,
            "all",
            None,
            &rows,
            &icons,
            false,
        )
        .expect("render should succeed");
        let data = &scratch;

        fn first_colored(data: &[u8], w: usize, y: usize) -> Option<usize> {
            (0..w).find(|&x| data[(y * w + x) * 4 + 3] != 0)
        }
        fn is_border(data: &[u8], w: usize, x: usize, y: usize) -> bool {
            let px = &data[(y * w + x) * 4..(y * w + x) * 4 + 4];
            // BGRA after the renderer's channel swap: blue channel dominates
            // for #89b4fa.
            px[0] > 200 && px[2] < 160
        }

        // Row 0 content starts at y=4, row 1 at y=108 (4 + 100 + 4 gap).
        let left0 = first_colored(data, 128, 40).expect("row 0 has content");
        let left1 = first_colored(data, 128, 150).expect("row 1 has content");
        assert!(
            (left0 as i32 - left1 as i32).abs() <= 1,
            "rows must be left-aligned: left0={left0} left1={left1}"
        );

        // The active highlight is on row 0's top-left corner...
        assert!(
            is_border(data, 128, 4, 4),
            "active border should start at (4,4)"
        );
        // ...and stops at the row's content width (50px), not the widget width.
        assert!(
            !is_border(data, 128, 56, 4),
            "border must not span the full widget"
        );
    }

    #[test]
    fn icons_are_centered_in_their_tiles() {
        // A 128px icon in a wide, short tile must be centred: the icon's left
        // edge sits at the tile centre minus half the icon width, never scaled
        // by the icon's own shrink factor.
        let mut icon = tiny_skia::Pixmap::new(128, 128).unwrap();
        icon.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        let icons = Arc::new(Mutex::new(IconCache::default()));
        icons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("test-icon", icon);

        let mut snap = Snapshot::default();
        snap.state
            .workspaces
            .workspaces
            .insert(1, ws(1, true, true));
        let mut win = test_window(10, 1, 1, 1, 800.0, 400.0, 0.0);
        win.app_id = Some("test-icon".into());
        snap.state.windows.windows.insert(10, win);
        let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
        let row = layout::build_row(ws, &snap.state.windows.windows, None, None, Some(&icons));

        let cfg = Config::default();
        let mut scratch = Vec::new();
        render_into(
            &mut scratch,
            200,
            100,
            1.0,
            &cfg.appearance,
            "current",
            Some(&row),
            &[],
            &icons,
            true,
        )
        .expect("render should succeed");
        let data = &scratch;

        fn red_px(data: &[u8], w: usize, x: usize, y: usize) -> bool {
            let px = &data[(y * w + x) * 4..(y * w + x) * 4 + 4];
            px[0] < 40 && px[2] > 200 // BGRA: red icon keeps B low, R high
        }
        fn leftmost_red(data: &[u8], w: usize, y: usize) -> Option<usize> {
            (0..w).find(|&x| red_px(data, w, x, y))
        }

        // Tile (after the 2px gap inset): x0=9, w=182, h=90; icon 63px
        // centred -> left edge at 68.5, so pixel 69 is the first fully
        // covered one.
        let left = leftmost_red(data, 200, 50).expect("icon should be visible");
        assert!(
            (68..70).contains(&left),
            "icon left edge must be centred, got {left}"
        );
        // Centre pixel is inside the icon...
        assert!(red_px(data, 200, 100, 50));
        // ...while the tile's own left area is the background colour.
        assert!(!red_px(data, 200, 20, 50));
    }

    #[test]
    fn all_mode_marks_last_focused_window() {
        // Rows stay left-aligned; the workspace's last-focused window is
        // outlined with the special border colour instead of being centred.
        let mut snap = Snapshot::default();
        let mut w1 = ws(1, true, true);
        w1.active_window_id = Some(11); // column 2 of workspace 1
        snap.state.workspaces.workspaces.insert(1, w1);
        snap.state
            .workspaces
            .workspaces
            .insert(2, ws(2, false, false));
        for (i, col) in [1usize, 2, 3].into_iter().enumerate() {
            snap.state.windows.windows.insert(
                10 + i as u64,
                test_window(10 + i as u64, 1, col, 1, 300.0, 400.0, 0.0),
            );
            snap.state.windows.windows.insert(
                20 + i as u64,
                test_window(20 + i as u64, 2, col, 1, 300.0, 400.0, 0.0),
            );
        }
        // Window 11 (column 2 of workspace 1) was the last focused one.
        snap.state.windows.windows.get_mut(&11).unwrap().is_focused = true;

        let ws1 = snap.state.workspaces.workspaces.get(&1).unwrap();
        let ws2 = snap.state.workspaces.workspaces.get(&2).unwrap();
        let row1 = layout::build_row(ws1, &snap.state.windows.windows, None, None, None);
        let row2 = layout::build_row(ws2, &snap.state.windows.windows, None, None, None);
        assert_eq!(
            row1.tiles.iter().filter(|t| t.is_last_focused).count(),
            1,
            "exactly one tile marked as last focused"
        );
        assert!(
            row2.tiles.iter().all(|t| !t.is_last_focused),
            "workspace without focus information has no special mark"
        );

        let rows = [
            RowView {
                is_active: true,
                row: row1,
            },
            RowView {
                is_active: false,
                row: row2,
            },
        ];
        let cfg = Config::default();
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let mut scratch = Vec::new();
        render_into(
            &mut scratch,
            150,
            212,
            1.0,
            &cfg.appearance,
            "all",
            None,
            &rows,
            &icons,
            false,
        )
        .expect("render should succeed");
        let data = &scratch;

        fn special_px(data: &[u8], w: usize, x: usize, y: usize) -> bool {
            let px = &data[(y * w + x) * 4..(y * w + x) * 4 + 4];
            // active_window_border_color #f38ba8 -> BGRA (168, 139, 243),
            // distinct from focused fill (250, 180, 137) and window fill.
            px[0] > 140 && px[0] < 200 && px[1] < 180 && px[2] > 230
        }
        fn special_range(data: &[u8], w: usize, y: usize) -> Option<(usize, usize)> {
            let mut lo = None;
            let mut hi = None;
            for x in 0..w {
                if special_px(data, w, x, y) {
                    lo.get_or_insert(x);
                    hi = Some(x);
                }
            }
            Some((lo?, hi?))
        }

        // Both rows are left-aligned: content starts at the same x.
        fn first_colored(data: &[u8], w: usize, y: usize) -> usize {
            (0..w)
                .find(|&x| data[(y * w + x) * 4 + 3] != 0)
                .unwrap_or(w)
        }
        let left0 = first_colored(data, 150, 40);
        let left1 = first_colored(data, 150, 150);
        assert!(
            (left0 as i32 - left1 as i32).abs() <= 1,
            "rows must be left-aligned: left0={left0} left1={left1}"
        );

        // Row 0: the special border outlines column 2 (left ≈ 79, clipped by
        // the widget's inner width at ≈ 146).
        let (lo, hi) = special_range(data, 150, 4).expect("special border visible");
        assert!((78..82).contains(&lo), "special border left edge, got {lo}");
        assert!(
            (142..147).contains(&hi),
            "special border right edge, got {hi}"
        );
        // Row 1: no special border.
        assert!(special_range(data, 150, 108).is_none());
    }

    #[test]
    fn floating_windows_are_placed_in_the_row() {
        let mut snap = Snapshot::default();
        let mut w1 = ws(1, true, true);
        w1.active_window_id = Some(20);
        snap.state.workspaces.workspaces.insert(1, w1);
        // Tiled window at column 1 (workspace x 0) shown at viewport x -200:
        // the viewport's left edge is at workspace x 200.
        snap.state
            .windows
            .windows
            .insert(10, test_window(10, 1, 1, 1, 400.0, 500.0, -200.0));
        // Floating window at viewport (100, 200), size 300x200.
        let mut float = test_window(20, 1, 1, 1, 300.0, 200.0, 100.0);
        float.layout.pos_in_scrolling_layout = None;
        float.layout.tile_pos_in_workspace_view = Some((100.0, 200.0));
        float.layout.window_size = (300, 200);
        float.is_focused = true;
        snap.state.windows.windows.insert(20, float);

        let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
        let row = layout::build_row(ws, &snap.state.windows.windows, None, None, None);

        let tiled = row.tiles.iter().find(|t| t.w == 400.0).unwrap();
        assert_eq!(tiled.x, 0.0);
        let fl = row.tiles.iter().find(|t| t.w == 300.0).unwrap();
        assert_eq!(
            fl.x, 300.0,
            "floating x = viewport offset (200) + view x (100)"
        );
        assert_eq!(fl.y, 200.0);
        assert_eq!(fl.w, 300.0);
        assert_eq!(fl.h, 200.0);
        assert!(fl.is_last_focused);
        // Scaling metrics come from the tiled layout only; the floating
        // window is an overlay and keeps its own extent.
        assert_eq!(row.total_width, 400.0);
        assert_eq!(row.max_height, 500.0);
        assert_eq!(row.float_width, 600.0);
        assert_eq!(row.float_height, 400.0);
        assert_eq!(row.scale_w(), 400.0);
        assert_eq!(row.scale_h(), 500.0);
    }

    #[test]
    fn floating_windows_do_not_rescale_other_rows() {
        // Moving a floating window in one workspace must not change the
        // scaling of any other row (or of the tiled windows in its own row).
        let mut snap = Snapshot::default();
        snap.state
            .workspaces
            .workspaces
            .insert(1, ws(1, true, true));
        snap.state
            .workspaces
            .workspaces
            .insert(2, ws(2, false, false));
        snap.state
            .windows
            .windows
            .insert(10, test_window(10, 1, 1, 1, 400.0, 500.0, 0.0));
        snap.state
            .windows
            .windows
            .insert(20, test_window(20, 2, 1, 1, 300.0, 400.0, 0.0));
        let mut float = test_window(21, 2, 1, 1, 300.0, 200.0, 0.0);
        float.layout.pos_in_scrolling_layout = None;
        float.layout.window_size = (300, 200);
        snap.state.windows.windows.insert(21, float);

        let cfg = Config::default();
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let mut scratch = Vec::new();

        fn row_tile_left(data: &[u8], w: usize, y: usize) -> Option<usize> {
            (0..w).find(|&x| data[(y * w + x) * 4 + 3] != 0)
        }

        let ws1 = snap.state.workspaces.workspaces.get(&1).unwrap();
        let ws2 = snap.state.workspaces.workspaces.get(&2).unwrap();

        // First frame: float near the top of workspace 2.
        snap.state.windows.windows.get_mut(&21).unwrap()
            .layout.tile_pos_in_workspace_view = Some((50.0, 10.0));
        {
            let rows = [
                RowView {
                    is_active: true,
                    row: layout::build_row(ws1, &snap.state.windows.windows, None, None, None),
                },
                RowView {
                    is_active: false,
                    row: layout::build_row(ws2, &snap.state.windows.windows, None, None, None),
                },
            ];
            render_into(
                &mut scratch,
                150,
                212,
                1.0,
                &cfg.appearance,
                "all",
                None,
                &rows,
                &icons,
                false,
            )
            .unwrap();
        }
        let row1_left = row_tile_left(&scratch, 150, 40).expect("row 1 tile");
        let row2_tiled_left = row_tile_left(&scratch, 150, 150).expect("row 2 tiled tile");

        // Second frame: float dragged far down, well beyond the tiled area.
        snap.state.windows.windows.get_mut(&21).unwrap()
            .layout.tile_pos_in_workspace_view = Some((50.0, 8000.0));
        {
            let rows = [
                RowView {
                    is_active: true,
                    row: layout::build_row(ws1, &snap.state.windows.windows, None, None, None),
                },
                RowView {
                    is_active: false,
                    row: layout::build_row(ws2, &snap.state.windows.windows, None, None, None),
                },
            ];
            render_into(
                &mut scratch,
                150,
                212,
                1.0,
                &cfg.appearance,
                "all",
                None,
                &rows,
                &icons,
                false,
            )
            .unwrap();
        }
        assert_eq!(
            row_tile_left(&scratch, 150, 40),
            Some(row1_left),
            "workspace 1 must not rescale when workspace 2's float moves"
        );
        assert_eq!(
            row_tile_left(&scratch, 150, 150),
            Some(row2_tiled_left),
            "workspace 2's tiled window must not rescale when its float moves"
        );
    }

    #[test]
    fn reused_scratch_does_not_ghost_old_pixels() {
        // Two renders into the same scratch buffer with different content:
        // regions that were covered by a tile in the first frame and are
        // empty in the second must be transparent, not stale pixels from the
        // previous frame.
        let mut snap = Snapshot::default();
        snap.state
            .workspaces
            .workspaces
            .insert(1, ws(1, true, true));
        let mut win = test_window(10, 1, 1, 1, 300.0, 200.0, 0.0);
        win.layout.pos_in_scrolling_layout = None;
        win.is_focused = true;
        win.layout.tile_pos_in_workspace_view = Some((100.0, 100.0));
        snap.state.windows.windows.insert(10, win);

        let cfg = Config::default(); // background_opacity = 0.0
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let mut scratch = Vec::new();

        fn tile_at(data: &[u8], w: usize, x: usize, y: usize) -> bool {
            data[(y * w + x) * 4 + 3] != 0
        }

        {
            let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
            let row = layout::build_row(ws, &snap.state.windows.windows, None, None, None);
            render_into(
                &mut scratch,
                200,
                200,
                1.0,
                &cfg.appearance,
                "current",
                Some(&row),
                &[],
                &icons,
                false,
            )
            .unwrap();
        }
        assert!(
            tile_at(&scratch, 200, 110, 110),
            "first frame draws the floating tile"
        );

        // Move the floating window far outside the widget.
        let win = snap.state.windows.windows.get_mut(&10).unwrap();
        win.layout.tile_pos_in_workspace_view = Some((2000.0, 2000.0));
        {
            let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
            let row = layout::build_row(ws, &snap.state.windows.windows, None, None, None);
            render_into(
                &mut scratch,
                200,
                200,
                1.0,
                &cfg.appearance,
                "current",
                Some(&row),
                &[],
                &icons,
                false,
            )
            .unwrap();
        }
        assert!(
            !tile_at(&scratch, 200, 110, 110),
            "moved tile must not ghost at its old position"
        );
    }
}
