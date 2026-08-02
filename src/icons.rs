//! Desktop-icon resolution with a lazy, cached lookup.
//!
//! Each `app_id` is resolved at most once (misses are cached too), so the
//! steady-state render path costs one hash lookup per window. The IPC thread
//! pre-warms icons for windows as they appear, keeping file I/O and decoding
//! off the render thread in the common case.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log::debug;
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Icon cache shared between the IPC thread (pre-warm) and the UI thread
/// (renderer). Locks are only held briefly per lookup.
pub type SharedIcons = Arc<Mutex<IconCache>>;

pub struct IconCache {
    cache: Mutex<HashMap<String, Option<Pixmap>>>,
    /// Lazily-built index of .desktop files: lowercase `Name=` /
    /// `StartupWMClass=` / file-stem -> .desktop path.
    desktop_index: Mutex<Option<HashMap<String, PathBuf>>>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            desktop_index: Mutex::new(None),
        }
    }
}

/// Convenience wrappers around a shared cache (std Mutex does not deref).
///
/// The mutex is locked exactly once here; do not call other locking entry
/// points while holding it (std Mutex is not reentrant).
pub fn has_icon(icons: &SharedIcons, app_id: &str) -> bool {
    let mut guard = icons.lock().unwrap_or_else(|e| e.into_inner());
    let IconCache {
        cache,
        desktop_index,
    } = &mut *guard;
    let Some(cache) = cache.get_mut().ok() else {
        return false;
    };
    resolve_into(cache, desktop_index, app_id).is_some()
}

pub fn with_icon<R>(icons: &SharedIcons, app_id: &str, f: impl FnOnce(Option<&Pixmap>) -> R) -> R {
    let mut cache = icons.lock().unwrap_or_else(|e| e.into_inner());
    let IconCache {
        cache,
        desktop_index,
    } = &mut *cache;
    let Some(cache) = cache.get_mut().ok() else {
        return f(None);
    };
    f(resolve_into(cache, desktop_index, app_id))
}

/// Look up the icon for `app_id`, resolving it on first use. Misses are
/// cached too, so repeated calls never rescan the disk. Callers must already
/// hold the cache lock (via `Mutex::get_mut`).
fn resolve_into<'a>(
    cache: &'a mut HashMap<String, Option<Pixmap>>,
    desktop_index: &Mutex<Option<HashMap<String, PathBuf>>>,
    app_id: &str,
) -> Option<&'a Pixmap> {
    if !cache.contains_key(app_id) {
        let icon = resolve_icon(app_id, desktop_index);
        if icon.is_none() {
            debug!("no desktop icon for {app_id:?}");
        }
        cache.insert(app_id.to_string(), icon);
    }
    cache.get(app_id).and_then(Option::as_ref)
}

fn resolve_icon(app_id: &str, index: &Mutex<Option<HashMap<String, PathBuf>>>) -> Option<Pixmap> {
    // The .desktop `Icon=` name first, then the app_id itself as an icon name
    // (many apps ship an icon that matches their app_id without a usable
    // desktop entry).
    let mut names = Vec::new();
    if let Some(desktop) = find_desktop_file(app_id, index) {
        if let Some(name) = desktop_icon_name(&desktop) {
            names.push(name);
        }
    }
    names.push(app_id.to_string());

    for name in names {
        if let Some(path) = find_icon_file(&name) {
            if let Some(pixmap) = decode_icon(&path) {
                return Some(pixmap);
            }
        }
    }
    None
}

/// Locate the .desktop file for an app_id, first by exact filename, then via
/// a lazily-built index over `Name=` / `StartupWMClass=` / file stems.
fn find_desktop_file(
    app_id: &str,
    index: &Mutex<Option<HashMap<String, PathBuf>>>,
) -> Option<PathBuf> {
    for dir in applications_dirs() {
        let candidate = dir.join(format!("{app_id}.desktop"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let mut index = index.lock().unwrap_or_else(|e| e.into_inner());
    if index.is_none() {
        *index = Some(build_desktop_index());
    }
    index.as_ref().unwrap().get(&app_id.to_lowercase()).cloned()
}

fn build_desktop_index() -> HashMap<String, PathBuf> {
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    for dir in applications_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            map.entry(stem.to_lowercase())
                .or_insert_with(|| path.clone());
            for key in desktop_keys(&path) {
                map.entry(key).or_insert_with(|| path.clone());
            }
        }
    }
    map
}

/// Lowercased lookup keys of a .desktop file: filename stem, `Name=` (plain
/// and localized) and `StartupWMClass=`.
fn desktop_keys(path: &Path) -> Vec<String> {
    let mut keys = Vec::new();
    let Ok(text) = fs::read_to_string(path) else {
        return keys;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key == "Name"
            || key == "StartupWMClass"
            || (key.starts_with("Name[") && key.ends_with(']'))
        {
            keys.push(value.trim().to_lowercase());
        }
    }
    keys
}

fn desktop_icon_name(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some(value) = line.strip_prefix("Icon=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn applications_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/applications"));
        dirs.push(PathBuf::from(&home).join(".local/share/flatpak/exports/share/applications"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            dirs.push(PathBuf::from(xdg).join("applications"));
        }
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Search the icon themes and pixmaps directories for an icon file.
fn find_icon_file(icon: &str) -> Option<PathBuf> {
    let icon = icon.trim();
    if icon.is_empty() {
        return None;
    }
    let icon_path = Path::new(icon);
    if icon_path.is_absolute() {
        if icon_path.is_file() {
            return Some(icon_path.to_path_buf());
        }
        for ext in ["png", "svg"] {
            let candidate = icon_path.with_extension(ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        return None;
    }

    let mut bases = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in data_dirs.split(':').filter(|s| !s.is_empty()) {
        bases.push(PathBuf::from(dir).join("icons"));
    }
    bases.push(PathBuf::from("/usr/share/pixmaps"));
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(&home).join(".local/share/pixmaps"));
    }

    for base in &bases {
        if let Some(path) = search_theme_dirs(base, icon) {
            return Some(path);
        }
        // Loose icons in pixmaps-style directories.
        for name in [icon, &icon.to_lowercase()] {
            for ext in ["png", "svg"] {
                let candidate = base.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn search_theme_dirs(base: &Path, icon: &str) -> Option<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return None;
    };
    let mut themes: Vec<PathBuf> = Vec::new();
    let mut hicolor = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("hicolor") {
            hicolor = Some(path);
        } else {
            themes.push(path);
        }
    }
    themes.sort();
    if let Some(h) = hicolor {
        themes.insert(0, h);
    }

    for theme in &themes {
        for sizedir in size_dirs_descending(theme) {
            if let Some(path) = search_subdirs(&sizedir, icon, true) {
                return Some(path);
            }
        }
        // Scalable and symbolic hierarchies (mostly SVG artwork).
        if let Some(path) = search_subdirs(&theme.join("scalable"), icon, true) {
            return Some(path);
        }
        if let Some(path) = search_subdirs(&theme.join("symbolic"), icon, false) {
            return Some(path);
        }
    }
    None
}

/// `{n}x{n}` directories under `theme`, largest first.
fn size_dirs_descending(theme: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(theme) else {
        return Vec::new();
    };
    let mut dirs: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((w, h)) = name.split_once('x') else {
            continue;
        };
        let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) else {
            continue;
        };
        if w != h || w == 0 {
            continue;
        }
        dirs.push((w, path));
    }
    dirs.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    dirs.into_iter().map(|(_, p)| p).collect()
}

/// Look for `{icon}.png` / `{icon}.svg` in every subdirectory of `dir`.
/// `apps` (the most common location) is checked first when `prefer_apps`.
fn search_subdirs(dir: &Path, icon: &str, prefer_apps: bool) -> Option<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    if prefer_apps {
        let apps = subdirs
            .iter()
            .find(|d| d.file_name().and_then(|n| n.to_str()) == Some("apps"));
        if let Some(apps) = apps {
            if let Some(path) = try_icon_in(apps, icon) {
                return Some(path);
            }
        }
    }
    for sub in &subdirs {
        if prefer_apps && sub.file_name().and_then(|n| n.to_str()) == Some("apps") {
            continue;
        }
        if let Some(path) = try_icon_in(sub, icon) {
            return Some(path);
        }
    }
    None
}

fn try_icon_in(dir: &Path, icon: &str) -> Option<PathBuf> {
    let lower = icon.to_lowercase();
    for name in [icon, lower.as_str()] {
        for ext in ["png", "svg"] {
            let candidate = dir.join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn decode_icon(path: &Path) -> Option<Pixmap> {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "png" => {
            let data = fs::read(path).ok()?;
            Pixmap::decode_png(&data).ok().and_then(cap_size)
        }
        "svg" => decode_svg(path),
        _ => None,
    }
}

/// Cap decoded icons at 128px per side. The widget never draws them larger,
/// and this keeps the cache small even when a theme ships 1024px artwork.
fn cap_size(pixmap: Pixmap) -> Option<Pixmap> {
    const MAX: u32 = 128;
    let (w, h) = (pixmap.width(), pixmap.height());
    if w <= MAX && h <= MAX {
        return Some(pixmap);
    }
    let k = (MAX as f32 / w.max(h) as f32).min(1.0);
    let nw = ((w as f32) * k).round().max(1.0) as u32;
    let nh = ((h as f32) * k).round().max(1.0) as u32;
    let mut out = Pixmap::new(nw, nh)?;
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..Default::default()
    };
    let t = Transform::from_scale(k, k);
    out.draw_pixmap(0, 0, pixmap.as_ref(), &paint, t, None);
    Some(out)
}

fn decode_svg(path: &Path) -> Option<Pixmap> {
    let data = fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }
    // Cap SVG rendering to keep the cache small; icons are drawn scaled down
    // anyway.
    let max = 128.0_f32;
    let w = (size.width()).min(max).ceil().clamp(1.0, max) as u32;
    let h = (size.height()).min(max).ceil().clamp(1.0, max) as u32;
    let mut pixmap = Pixmap::new(w, h)?;
    let scale = Transform::from_scale(w as f32 / size.width(), h as f32 / size.height());
    let mut pm = pixmap.as_mut();
    resvg::render(&tree, scale, &mut pm);
    Some(pixmap)
}

pub fn draw_icon(pix: &mut Pixmap, icon: &Pixmap, scale: f32, x: f32, y: f32, w: f32, h: f32) {
    let iw = icon.width() as f32;
    let ih = icon.height() as f32;
    if iw <= 0.0 || ih <= 0.0 || w <= 0.0 || h <= 0.0 {
        return;
    }
    // Fit into 70% of the shorter tile side, never upscale (small icons look
    // blurry when enlarged).
    let fit = (w.min(h)) * 0.7;
    let k = (fit / iw.max(ih)).min(1.0);
    let dw = iw * k;
    let dh = ih * k;
    if dw < 1.0 || dh < 1.0 {
        return;
    }
    let dx = x + (w - dw) / 2.0;
    let dy = y + (h - dh) / 2.0;
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..Default::default()
    };
    // map texture -> device: scale by k (then physical scale), then translate
    // to the tile centre in device pixels.
    let t = Transform::from_scale(k * scale, k * scale).pre_translate(dx * scale, dy * scale);
    pix.draw_pixmap(0, 0, icon.as_ref(), &paint, t, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resolves_real_desktop_icons() {
        // Best-effort check against the host system: resolve icons for a few
        // desktop entries that actually exist here, without failing when the
        // system layout differs.
        let shared: SharedIcons = Arc::new(Mutex::new(IconCache::default()));
        let mut checked = 0;
        for dir in applications_dirs() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let width = with_icon(&shared, stem, |i| i.map(|p| p.width()));
                if width.is_some() {
                    assert!(has_icon(&shared, stem));
                    checked += 1;
                }
                if checked >= 3 {
                    return;
                }
            }
        }
        assert!(checked > 0, "no resolvable icon found on this system");
    }

    #[test]
    fn resolution_hit_rate_on_this_system() {
        // Diagnostic: how many desktop entries resolve to an icon here? Low
        // hit rates point at a matching bug rather than a system quirk.
        let shared: SharedIcons = Arc::new(Mutex::new(IconCache::default()));
        let mut total = 0;
        let mut hits = 0;
        let mut missed: Vec<String> = Vec::new();
        for dir in applications_dirs() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if has_icon(&shared, stem) {
                    hits += 1;
                } else if missed.len() < 20 {
                    missed.push(stem.to_string());
                }
                total += 1;
            }
        }
        eprintln!("icon hit rate: {hits}/{total}; first misses: {:?}", missed);
        assert!(total > 0);
        // A sane desktop system resolves the vast majority of its own entries.
        assert!(hits * 100 >= total * 50, "hit rate too low: {hits}/{total}");
    }

    #[test]
    fn shared_lookup_locks_exactly_once() {
        // Regression test: the shared wrappers used to lock the mutex and then
        // lock it again through the inner method, deadlocking on the very
        // first lookup (std Mutex is not reentrant). Run the exact production
        // entry points on a thread and fail if they do not return.
        let shared: SharedIcons = Arc::new(Mutex::new(IconCache::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let has = has_icon(&shared, "nirimap-test-nonexistent");
            let via_with = with_icon(&shared, "nirimap-test-nonexistent", |i| i.is_some());
            tx.send((has, via_with)).unwrap();
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok((has, via_with)) => {
                assert!(!has);
                assert!(!via_with);
            }
            Err(_) => panic!("icon lookup deadlocked"),
        }
        worker.join().unwrap();
    }
}
