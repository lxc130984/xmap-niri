use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::Result;
use calloop::channel::Sender;
use log::{info, warn};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::ipc::UiMsg;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub display: DisplayConfig,
    pub appearance: AppearanceConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Per-workspace row height in logical pixels.
    pub height: u32,
    /// Maximum widget width as a fraction of the output width (0.0 - 1.0).
    pub max_width_percent: f64,
    /// Maximum widget height as a fraction of the output height (all mode).
    pub max_height_percent: f64,
    /// Widget anchor: top-left, top-center, top-right, bottom-left, bottom-center,
    /// bottom-right, center.
    pub anchor: String,
    /// Horizontal margin from the anchored edge, logical pixels.
    pub margin_x: i32,
    /// Vertical margin from the anchored edge, logical pixels.
    pub margin_y: i32,
    /// "current" shows only the focused workspace, "all" stacks every workspace.
    pub mode: String,
    /// Follow the focused output; if false, use the first output.
    pub follow_focus: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub background: String,
    pub background_opacity: f64,
    pub window_color: String,
    pub focused_color: String,
    pub border_color: String,
    pub border_width: f64,
    pub border_radius: f64,
    pub gap: f64,
    pub window_opacity: f64,
    pub focused_opacity: f64,
    /// Draw the application's desktop icon in each window tile instead of a
    /// plain rectangle. Windows without a resolvable icon are skipped.
    #[serde(default = "default_true")]
    pub show_icons: bool,
    pub workspace_gap: f64,
    pub active_workspace_border_color: String,
    pub active_workspace_border_width: f64,
    /// Border colour of the workspace's last-focused window.
    #[serde(default = "default_active_window_border_color")]
    pub active_window_border_color: String,
    /// Border width of the workspace's last-focused window.
    #[serde(default = "default_active_window_border_width")]
    pub active_window_border_width: f64,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            background: "#1e1e2e".into(),
            background_opacity: 0.0,
            window_color: "#45475a".into(),
            focused_color: "#89b4fa".into(),
            border_color: "#6c7086".into(),
            border_width: 1.0,
            border_radius: 2.0,
            gap: 2.0,
            window_opacity: 0.7,
            focused_opacity: 1.0,
            show_icons: true,
            workspace_gap: 4.0,
            active_workspace_border_color: "#89b4fa".into(),
            active_workspace_border_width: 2.0,
            active_window_border_color: "#f38ba8".into(),
            active_window_border_width: 2.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display: DisplayConfig {
                height: 100,
                max_width_percent: 0.5,
                max_height_percent: 0.8,
                anchor: "top-right".into(),
                margin_x: 10,
                margin_y: 10,
                mode: "current".into(),
                follow_focus: true,
            },
            appearance: AppearanceConfig {
                ..AppearanceConfig::default()
            },
        }
    }
}

impl Config {
    /// Path of the config file: $XDG_CONFIG_HOME/nirimap/config.toml
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nirimap")
            .join("config.toml")
    }

    /// Load the config; writes the default config on first run.
    /// A parse failure falls back to defaults (and logs), so a broken edit
    /// never kills the process.
    pub fn load() -> Result<Config> {
        let path = Self::path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(&path, DEFAULT_CONFIG)?;
            info!("wrote default config to {}", path.display());
        }
        let text = fs::read_to_string(&path)?;
        match toml::from_str(&text) {
            Ok(cfg) => Ok(cfg),
            Err(err) => {
                warn!("failed to parse {}: {err}; using defaults", path.display());
                Ok(Config::default())
            }
        }
    }
}

/// Watch the config file and send a reload message on change.
pub fn spawn_config_watcher(path: &Path, tx: Sender<UiMsg>) -> Result<()> {
    let path = path.to_path_buf();
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    thread::Builder::new()
        .name("nirimap-config".into())
        .spawn(move || {
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        if ev.paths.iter().any(|p| p == &path) {
                            tx.send(UiMsg::ConfigReload).ok();
                        }
                    }
                }) {
                    Ok(w) => w,
                    Err(err) => {
                        warn!("failed to start config watcher: {err}");
                        return;
                    }
                };
            if let Err(err) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
                warn!("failed to watch config directory: {err}");
                return;
            }
            loop {
                thread::park();
            }
        })?;
    Ok(())
}

/// Parse "#rrggbb" into (r, g, b) floats in 0.0 - 1.0.
pub fn parse_hex(s: &str) -> Option<(f32, f32, f32)> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some((
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
    ))
}

fn default_true() -> bool {
    true
}

fn default_active_window_border_color() -> String {
    "#f38ba8".into()
}

fn default_active_window_border_width() -> f64 {
    2.0
}

const DEFAULT_CONFIG: &str = r##"# nirimap configuration

[display]
# Height of one workspace row (logical pixels).
height = 100
# Maximum widget width as a fraction of the output width.
max_width_percent = 0.5
# Maximum widget height as a fraction of the output height ("all" mode only).
max_height_percent = 0.8
# Widget position: top-left, top-center, top-right, bottom-left, bottom-center,
# bottom-right, center.
anchor = "top-right"
margin_x = 10
margin_y = 10
# "current" = show only the focused workspace; "all" = stack every workspace.
mode = "current"
# Follow the focused output. When false, the first output is used.
follow_focus = true

[appearance]
background = "#1e1e2e"
background_opacity = 0.0
window_color = "#45475a"
focused_color = "#89b4fa"
border_color = "#6c7086"
border_width = 1
border_radius = 2
gap = 2
window_opacity = 0.7
focused_opacity = 1.0
show_icons = true
workspace_gap = 4
active_workspace_border_color = "#89b4fa"
active_workspace_border_width = 2
# Border marking the workspace's last-focused window in the "all" preview.
active_window_border_color = "#f38ba8"
active_window_border_width = 2
"##;
