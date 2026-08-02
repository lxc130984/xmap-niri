//! CPU rendering of the minimap into a premultiplied BGRA8888 buffer
//! (wl_shm ARGB8888 byte order).

use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::config::{parse_hex, AppearanceConfig};
use crate::icons::{self, draw_icon, SharedIcons};
use crate::layout::Row;

/// Padding around the content inside the widget, logical pixels.
pub const PADDING: f32 = 4.0;

/// One workspace row to draw in "all" mode.
pub struct RowView<'a> {
    pub is_active: bool,
    pub row: &'a Row,
}

/// Render the minimap.
///
/// `phys_w`/`phys_h` are buffer pixels, `scale` is the integer buffer scale and
/// `viewport_width` is the focused output's logical width (used to align the
/// viewport in "all" mode).
#[allow(clippy::too_many_arguments)]
pub fn render(
    phys_w: u32,
    phys_h: u32,
    scale: f32,
    cfg: &AppearanceConfig,
    mode: &str,
    focused: Option<&Row>,
    rows: &[RowView<'_>],
    viewport_width: f32,
    icons: &SharedIcons,
    show_icons: bool,
) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(phys_w, phys_h)?;
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
        draw_all(
            &mut pixmap,
            s,
            cfg,
            rows,
            log_w,
            log_h,
            viewport_width,
            icons,
            show_icons,
        );
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
    let mut data = pixmap.data().to_vec();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(data)
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
    let Some(row) = focused else { return };
    if !row.has_content() {
        return;
    }
    let inner_h = (log_h - 2.0 * PADDING).max(0.0);
    if inner_h <= 0.0 || row.max_height <= 0.0 {
        return;
    }
    let k = inner_h / row.max_height as f32;
    let content_w = row.total_width as f32 * k;
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
    rows: &[RowView<'_>],
    log_w: f32,
    log_h: f32,
    viewport_width: f32,
    icons: &SharedIcons,
    show_icons: bool,
) {
    if rows.is_empty() {
        return;
    }
    let n = rows.len() as f32;
    let gap = cfg.workspace_gap as f32;
    let row_h = ((log_h - 2.0 * PADDING - (n - 1.0) * gap) / n).max(1.0);
    let global_max = rows
        .iter()
        .filter(|r| r.row.has_content())
        .map(|r| r.row.max_height as f32)
        .fold(0.0_f32, f32::max);
    if global_max <= 0.0 {
        return;
    }
    let k = row_h / global_max;

    // Combined extent of the viewport-anchored content across all rows.
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    for r in rows {
        if r.row.has_content() {
            left = left.min(-r.row.align_x as f32);
            right = right.max((r.row.total_width - r.row.align_x) as f32);
        }
    }
    let inner_w = (log_w - 2.0 * PADDING).max(0.0);
    let anchor_x = if left.is_finite() {
        let content_w = (right - left) * k;
        if content_w > inner_w {
            // Too wide: keep the viewport itself visible instead of the left edge.
            let vp_scaled = viewport_width * k;
            PADDING + ((inner_w - vp_scaled).max(0.0)) / 2.0
        } else {
            PADDING - left * k
        }
    } else {
        PADDING
    };

    let mut y = PADDING;
    for r in rows {
        if r.is_active && cfg.active_workspace_border_width > 0.0 {
            let mut paint = Paint::default();
            match parse_hex(&cfg.active_workspace_border_color) {
                Some((cr, cg, cb)) => set_paint_color(&mut paint, cr, cg, cb, 1.0),
                None => set_paint_color(&mut paint, 0.54, 0.71, 0.98, 1.0),
            }
            let rect = rect_min(PADDING, y, inner_w, row_h);
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
                let x = anchor_x + (t.x - r.row.align_x) as f32 * k;
                let ty = y + t.y as f32 * k;
                draw_tile(
                    pix,
                    s,
                    cfg,
                    x,
                    ty,
                    t.w as f32 * k,
                    t.h as f32 * k,
                    t.focused,
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

    if cfg.border_width > 0.0 {
        if let Some((r, g, b)) = parse_hex(&cfg.border_color) {
            let mut paint = Paint::default();
            set_paint_color(&mut paint, r, g, b, 1.0);
            pix.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width: cfg.border_width as f32,
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

    fn test_window(id: u64, col: usize, tile: usize, w: f64, h: f64) -> Window {
        Window {
            id,
            title: Some(format!("win{id}")),
            app_id: Some("test".into()),
            pid: None,
            workspace_id: Some(1),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((col, tile)),
                tile_size: (w, h),
                window_size: (w as i32, h as i32),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn renders_pixels_for_tiled_windows() {
        let mut snap = Snapshot::default();
        snap.state.workspaces.workspaces.insert(
            1,
            Workspace {
                id: 1,
                idx: 0,
                name: Some("1".into()),
                output: Some("eDP-1".into()),
                is_active: true,
                is_focused: true,
                is_urgent: false,
                active_window_id: Some(10),
            },
        );
        snap.state
            .windows
            .windows
            .insert(10, test_window(10, 1, 1, 800.0, 600.0));
        snap.state
            .windows
            .windows
            .insert(11, test_window(11, 1, 2, 800.0, 400.0));

        let ws = snap.state.workspaces.workspaces.get(&1).unwrap();
        let row = layout::build_row(ws, &snap.state.windows.windows, None);
        assert!(row.has_content());
        assert_eq!(row.tiles.len(), 2);

        let cfg = Config::default();
        let icons = Arc::new(Mutex::new(IconCache::default()));
        let data = render(
            200,
            200,
            1.0,
            &cfg.appearance,
            "current",
            Some(&row),
            &[],
            1920.0,
            &icons,
            false,
        )
        .expect("render should succeed");
        assert_eq!(data.len(), 200 * 200 * 4);
        // With show_icons=false, tiles draw colored rects: at least one pixel
        // must be non-transparent.
        assert!(data.chunks_exact(4).any(|px| px[3] != 0));

        // Icon mode with an unresolvable app_id filters the row to empty.
        let row = layout::build_row(ws, &snap.state.windows.windows, Some(&icons));
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
}
