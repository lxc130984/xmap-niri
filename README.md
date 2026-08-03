# nirimap

A lightweight minimap overlay for the [niri](https://github.com/YaLTeR/niri)
Wayland compositor, written in Rust with a focus on low CPU and memory usage.

It renders a small map of the tiled windows on the focused workspace (or every
workspace) into a top-right layer-shell overlay, and keeps out of your way the
rest of the time.

## Features

- **Right-top minimap** via `zwlr-layer-shell` (overlay layer, click-through).
- Two modes:
  - `current` — show only the focused workspace.
  - `all` — stack every non-empty workspace, one row each.
- Follows the focused output, tracks output scale and window positions in real
  time through the niri IPC event stream.
- **Desktop icons**: window tiles can show the app's .desktop icon (PNG or
  SVG) instead of a plain rectangle; windows without a resolvable icon are
  skipped. Icons are resolved lazily once per app and cached.
- **Last-focus marker**: in "all" mode, each workspace's last-focused window
  is outlined with a distinct border colour, so you can spot where you left
  off in every workspace at a glance.
- Floating windows are rendered too, placed from their viewport position
  (they are skipped only while scrolled outside the view, where niri stops
  reporting a position). They are drawn as overlays and never change the
  minimap's scaling, so dragging one does not resize the other rows.
- Fully CPU-side rendering with tiny-skia; no GPU context, no shaders.
- Event-driven redraws paced by compositor frame callbacks, with a content
  hash so events that don't change the minimap (urgency, focus timestamps,
  keyboard layout churn) never trigger a repaint.
- Always visible; no hide logic, no polling timers, no idle wake-ups.
- Live-reloads `config.toml` while running.

## Why it is light

- **Small buffers.** The widget is a few hundred logical pixels at most, and
  shm buffers are shared-memory only — no GPU allocations.
- **Double-buffered, reused.** At most 2 buffers are kept; the compositor's
  `wl_buffer.release` returns them to the pool instead of reallocating, and
  oversized buffers are reclaimed when the widget shrinks.
- **No per-frame allocation in the hot path.** The render pixmap is a reused
  scratch buffer, so steady-state redraws allocate nothing.
- **Icon lookup is cached.** Each `app_id` is resolved at most once (misses
  included), off the render thread; steady-state drawing is one hash lookup
  and a small scaled blit per window.
- **One IPC thread, one UI thread.** No polling loops; niri pushes state
  changes over its socket, irrelevant events are filtered in the IPC thread,
  and the UI thread only wakes when the minimap could actually change.
- **Idle by default:** with no events, the process sleeps with zero redraws.

## Build

```sh
cargo build --release
```

The binary is `target/release/nirimap`. Run it inside a niri session:

```sh
target/release/nirimap
```

Requires `WAYLAND_DISPLAY` and `NIRI_SOCKET` (both are set automatically by
niri). If niri restarts, the IPC thread reconnects on its own.

## Configuration

The default config is written to
`$XDG_CONFIG_HOME/nirimap/config.toml` (`~/.config/nirimap/config.toml`) on
first run and hot-reloaded on change. Highlights:

```toml
[display]
height = 100            # height of one workspace row (logical px)
anchor = "top-right"    # top-left | top-center | top-right | bottom-left | ...
mode = "current"        # "current" or "all"
follow_focus = true

[appearance]
background = "#1e1e2e"
window_color = "#45475a"
focused_color = "#89b4fa"
window_opacity = 0.7
show_icons = true        # draw .desktop icons in tiles; skip icon-less windows
active_window_border_color = "#f38ba8"  # last-focused window outline
active_window_border_width = 2
```

## How it works

- `src/ipc.rs` — a dedicated thread owns the niri IPC socket, applies every
  event to an `EventStreamState`, and pushes a small `UiMsg` to the UI thread.
- `src/layout.rs` — turns the niri window/workspace state into minimap rows,
  using each tiled window's column/tile position.
- `src/render.rs` — tiny-skia rasterization (RGBA) with a final BGR(A) byte
  swap for `wl_shm` ARGB8888.
- `src/icons.rs` — lazy .desktop icon resolution (PNG via tiny-skia, SVG via
  resvg) with a shared cache.
- `src/app.rs` — the layer-shell surface, shm buffer pool, and frame-callback
  pacing.
