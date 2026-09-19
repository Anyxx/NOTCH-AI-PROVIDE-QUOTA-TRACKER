#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod autostart;
mod config;
mod doctor;
mod focus;
mod hooks_install;
mod i18n;
mod server;
mod state;
mod tray;
mod usage;
mod codex;
mod cursor;
mod antigravity;
mod commandcode;
mod router9;
mod deepseek;
mod secrets;
mod settings;
mod glyphs;
mod activity;
mod diag;
mod watcher;

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Hand-bumped build tag, written to run.log at startup so a log can always be matched to the exe that wrote it.
pub const BUILD: &str = "r41";
/// The notch window, logical px (mirrored in ui/notch.html, WIN). The approach Bloom uses
/// (github.com/sehajveersingh2005/bloom): the window keeps one size per edge and never resizes while
/// the island animates. The island is an element inside it that springs between shapes, and the
/// transparent rest of the window lets clicks through (set_click_through, driven by
/// start_hit_test). Resizing the window mid-animation made the shape jump and wander.
const WIN_WIDE: (f64, f64) = (620.0, 520.0); // top / bottom edge, or floating
const WIN_TALL: (f64, f64) = (480.0, 640.0); // left / right edge
/// Floating, the closed pill hangs at the top of its window; this is the pill's centre, which is
/// where the saved free position points
const FREE_ANCHOR_Y: f64 = 19.0;
/// The closed island (mirrored in ui/notch.html, geom): lying along a top/bottom edge, standing on a side
const PILL_WIDE: (f64, f64) = (200.0, 32.0);
const PILL_TALL: (f64, f64) = (32.0, 140.0);

/// Open (true) or collapsed to the handle (false). The page drives this through `notch_expand`.
static EXPANDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 0 = plain handle, 1 = Live Activity pill, 2 = alert pill. Only matters while closed. Set by `notch_peek`.
static PEEK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub struct AppState {
    pub store: Mutex<state::Store>,
    pub cfg: Mutex<config::Config>,
    pub usage: Mutex<usage::UsageSnapshot>,
    /// Codex snapshot (same UsageSnapshot shape; status may also be none/absent)
    pub codex: Mutex<usage::UsageSnapshot>,
    pub cursor: Mutex<usage::UsageSnapshot>,
    pub antigravity: Mutex<usage::UsageSnapshot>,
    pub commandcode: Mutex<usage::UsageSnapshot>,
    pub router9: Mutex<usage::UsageSnapshot>,
    pub deepseek: Mutex<usage::UsageSnapshot>,
    /// Provider glyph cache, collected at launch and again on a tray refresh
    pub glyphs: Mutex<std::collections::HashMap<String, glyphs::Glyph>>,
    /// Working state of the non-Claude providers (Cursor reports it; Codex and Antigravity are inferred from recent writes)
    pub activity: Mutex<Vec<activity::Activity>>,
}

fn resolved_lang(raw: &str) -> String {
    if raw == "auto" {
        i18n::resolve_auto().to_string()
    } else {
        raw.to_string()
    }
}

pub fn broadcast(app: &AppHandle) {
    let st = app.state::<AppState>();
    let snap = {
        let store = st.store.lock().unwrap();
        let cfg = st.cfg.lock().unwrap();
        store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
    };
    let _ = app.emit("state", &snap);
}

/// How many provider cells the pill will draw: Claude always has one, the rest only once they report
/// something other than "absent" (same rule as `providers()` in the page).
#[allow(dead_code)] // the window no longer sizes to the cell count; kept for diagnostics
fn visible_providers(app: &AppHandle) -> usize {
    let st = app.state::<AppState>();
    let hidden = st.cfg.lock().map(|c| c.hidden_providers.clone()).unwrap_or_default();
    let shown = |id: &str| !hidden.iter().any(|h| h == id);
    let mut n = usize::from(shown("claude"));
    for (id, m) in [
        ("codex", &st.codex),
        ("cursor", &st.cursor),
        ("gemini", &st.antigravity),
        ("commandcode", &st.commandcode),
        ("router9", &st.router9),
        ("deepseek", &st.deepseek),
    ] {
        if shown(id) && m.lock().map(|s| s.status != "absent").unwrap_or(false) {
            n += 1;
        }
    }
    n.max(1) // the page never hides every cell: Claude comes back when nothing else is left
}

/// Colour themes, mirrored in ui/notch.html (THEMES) and the settings window. "dark" is the old
/// name of midnight, still accepted from existing config files.
pub const THEMES: [(&str, &str); 7] = [
    ("midnight", "Midnight"),
    ("graphite", "Graphite"),
    ("ocean", "Ocean"),
    ("sunset", "Sunset"),
    ("forest", "Forest"),
    ("rose", "Rose"),
    ("glass", "Glass"),
];
fn theme_ok(t: &str) -> bool {
    t == "dark" || THEMES.iter().any(|(id, _)| *id == t)
}

/// Provider order, hidden providers and theme, from the settings window's layout card
#[tauri::command]
fn save_layout(app: AppHandle, order: Vec<String>, hidden: Vec<String>, theme: String) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.provider_order = order;
        c.hidden_providers = hidden;
        if theme_ok(&theme) {
            c.theme = theme;
        }
        config::save(&c);
    }
    emit_config(&app);
    place_notch(&app);
}

/// One setting changed from the island's own Settings tab. The notch never takes focus, but it
/// takes clicks, so everything there is a button, a switch or a swatch — nothing to type.
#[tauri::command]
fn set_pref(app: AppHandle, key: String, value: serde_json::Value) -> Result<(), String> {
    let bad = || format!("bad value for {key}");
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        match key.as_str() {
            "theme" => {
                let t = value.as_str().ok_or_else(bad)?;
                if !theme_ok(t) {
                    return Err(bad());
                }
                c.theme = t.to_string();
            }
            "edge" => {
                let e = value.as_str().ok_or_else(bad)?;
                if !["top", "bottom", "left", "right"].contains(&e) {
                    return Err(bad());
                }
                c.edge = e.to_string();
                c.notch_y = 0.5;
                c.drag_enabled = false; // choosing a spot means docking there
            }
            "free" => c.drag_enabled = value.as_bool().ok_or_else(bad)?,
            "live_activity" => c.live_activity = value.as_bool().ok_or_else(bad)?,
            "hide_fullscreen" => c.hide_fullscreen = value.as_bool().ok_or_else(bad)?,
            "alert_levels" => {
                let v: Vec<u32> = serde_json::from_value(value).map_err(|_| bad())?;
                c.alert_levels = v.into_iter().filter(|p| (1..=100).contains(p)).collect();
            }
            "scale" => c.scale = value.as_f64().ok_or_else(bad)?.clamp(0.7, 1.6),
            "opacity" => c.opacity = value.as_f64().ok_or_else(bad)?.clamp(0.15, 1.0),
            "hidden_providers" => c.hidden_providers = serde_json::from_value(value).map_err(|_| bad())?,
            _ => return Err(format!("unknown setting {key}")),
        }
        config::save(&c);
    }
    emit_config(&app);
    place_notch(&app);
    tray::refresh_menu(&app);
    Ok(())
}

fn dock_edge(edge: &str) -> &'static str {
    match edge {
        "left" => "left",
        "right" => "right",
        "bottom" => "bottom",
        _ => "top",
    }
}

/// Logical window size: fixed per edge, whatever the island inside is doing
fn window_size(edge: &str, notch_scale: f64) -> (f64, f64) {
    let (w, h) = if edge == "left" || edge == "right" { WIN_TALL } else { WIN_WIDE };
    (w * notch_scale, h * notch_scale)
}

/// Where the island sits in its window, for the page: the edge ("free" when floating) and the
/// island's centre along that edge as a fraction of the window. Docked near a screen corner the
/// window is kept on the monitor, so the island is off-centre in it — this says by how much.
static PLACEMENT: Mutex<(String, f64)> = Mutex::new((String::new(), 0.5));

#[tauri::command]
fn get_placement() -> serde_json::Value {
    let p = PLACEMENT.lock().unwrap();
    serde_json::json!({ "edge": p.0, "along": p.1 })
}

/// Places the notch: docked against its edge (the pill's centre held at the saved ratio along that
/// edge, so growing and shrinking never makes it wander), or free-floating around the saved centre.
/// The monitor the notch lives on: the one it was last dropped on (saved by name), or the primary
/// one when that screen is gone — unplugged, or renamed after a driver update. Everything used to
/// assume the primary monitor, so a notch dragged onto a second screen snapped straight back.
fn notch_monitor(app: &AppHandle) -> Option<tauri::Monitor> {
    let want = app.state::<AppState>().cfg.lock().ok().and_then(|c| c.monitor.clone());
    if let (Some(name), Ok(list)) = (want, app.available_monitors()) {
        if let Some(m) = list.into_iter().find(|m| m.name().map(|n| *n == name).unwrap_or(false)) {
            return Some(m);
        }
    }
    app.primary_monitor().ok().flatten()
}

/// The monitor containing a physical point (where the notch was let go), else the primary one
fn monitor_at(app: &AppHandle, x: i32, y: i32) -> Option<tauri::Monitor> {
    if let Ok(list) = app.available_monitors() {
        let hit = list.into_iter().find(|m| {
            let (p, s) = (m.position(), m.size());
            x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
        });
        if hit.is_some() {
            return hit;
        }
    }
    app.primary_monitor().ok().flatten()
}

pub fn place_notch(app: &AppHandle) {
    let Some(w) = app.get_webview_window("notch") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    let (free_move, free_centre, notch_scale, edge, ratio) = {
        let st = app.state::<AppState>();
        let c = st.cfg.lock().unwrap();
        (
            c.drag_enabled,
            c.bar_x.zip(c.bar_y),
            c.scale.clamp(0.7, 1.6),
            c.edge.clone(),
            c.notch_y.clamp(0.0, 1.0),
        )
    };
    let Some(mon) = notch_monitor(app) else { return };
    // Mixed-DPI setups: the physical size is pinned straight from the monitor's own scale factor,
    // then pinned once more if the window still reports another size after moving there.
    let ms = mon.scale_factor();
    let edge = if free_move { "free" } else { dock_edge(&edge) };
    let (nw, nh) = window_size(edge, notch_scale);
    let (ww, wh) = ((nw * ms).round() as i32, (nh * ms).round() as i32);
    let target = tauri::PhysicalSize::new(ww as u32, wh as u32);
    let _ = w.set_size(target);
    let (mx, my) = (mon.position().x, mon.position().y);
    let (mw, mh) = (mon.size().width as i32, mon.size().height as i32);
    // Along its edge the island sits at the saved ratio. The window is centred on that point but
    // kept on the monitor, so near a corner the island is off-centre in its window — `along`
    // tells the page where. Across the edge the window is flush, so the island grows out of it.
    let (x, y, along) = match edge {
        "free" => {
            let (cx, cy) = free_centre.unwrap_or((mx + mw / 2, my + mh / 3));
            let anchor = (FREE_ANCHOR_Y * notch_scale * ms).round() as i32;
            let (x, y) = clamp_to_virtual_screen(cx - ww / 2, cy - anchor, ww, wh);
            (x, y, (cx - x) as f64 / ww.max(1) as f64)
        }
        "top" | "bottom" => {
            let c = mx as f64 + mw as f64 * ratio;
            let x = ((c - ww as f64 / 2.0).round() as i32).clamp(mx, mx + (mw - ww).max(0));
            let y = if edge == "top" { my } else { my + mh - wh };
            (x, y, (c - x as f64) / ww.max(1) as f64)
        }
        _ => {
            let c = my as f64 + mh as f64 * ratio;
            let y = ((c - wh as f64 / 2.0).round() as i32).clamp(my, my + (mh - wh).max(0));
            let x = if edge == "left" { mx } else { mx + mw - ww };
            (x, y, (c - y as f64) / wh.max(1) as f64)
        }
    };
    let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    if w.outer_size().map(|s| s.width != target.width).unwrap_or(false) {
        let _ = w.set_size(target);
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let along = along.clamp(0.0, 1.0);
    *PLACEMENT.lock().unwrap() = (edge.to_string(), along);
    let _ = app.emit("placement", serde_json::json!({ "edge": edge, "along": along }));
    // Placement log line: the first thing to check when the notch is not visible (appended, never rewritten)
    applog(&format!(
        "notch placed build={BUILD}: edge={edge} along={along:.3} pos=({x},{y}) size=({ww}x{wh}) inner={:?} win_scale={scale} mon_scale={ms} monitor=({mx},{my} {mw}x{mh})",
        w.inner_size().map(|s| (s.width, s.height)).unwrap_or((0, 0)),
    ));
}

/// Rectangles the page reports for the island and its detail card: physical px relative to the
/// window's top-left. Outside them the window lets clicks through to whatever is underneath.
static ISLAND_RECTS: Mutex<Vec<[f64; 4]>> = Mutex::new(Vec::new());

#[tauri::command]
fn update_island_rect(rects: Vec<[f64; 4]>) {
    *ISLAND_RECTS.lock().unwrap() = rects;
}

/// Click-through and hover, Bloom's way: ~33 times a second the cursor is compared against the
/// island's rectangles. Outside, the window ignores the mouse (so the large transparent window never
/// blocks the desktop); inside, it takes it, and the page is told the island is hovered. Once in, the
/// margin grows a little (hysteresis) so the boundary cannot flicker. The cursor pressed against the
/// screen edge right at a docked island counts as on it — that is where a pointer stops.
fn start_hit_test(app: AppHandle) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut ignoring: Option<bool> = None;
        let mut hovered = false;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let Some(w) = app.get_webview_window("notch") else { continue };
            if DRAGGING.load(Ordering::SeqCst) {
                if ignoring != Some(false) {
                    set_click_through(&w, false);
                    ignoring = Some(false);
                }
                continue;
            }
            let (Ok(pos), Ok(size), Ok(cur)) = (w.outer_position(), w.outer_size(), app.cursor_position()) else { continue };
            let (lx, ly) = (cur.x - pos.x as f64, cur.y - pos.y as f64);
            let (ww, wh) = (size.width as f64, size.height as f64);
            let rects = ISLAND_RECTS.lock().unwrap().clone();
            let pad = if hovered { 14.0 } else { 3.0 };
            let mut inside = rects.iter().any(|r| lx >= r[0] - pad && ly >= r[1] - pad && lx < r[0] + r[2] + pad && ly < r[1] + r[3] + pad);
            if !inside {
                if let Some(r) = rects.first() {
                    let edge = PLACEMENT.lock().unwrap().0.clone();
                    let band = 40.0;
                    let along_x = lx >= r[0] - band && lx < r[0] + r[2] + band;
                    let along_y = ly >= r[1] - band && ly < r[1] + r[3] + band;
                    inside = match edge.as_str() {
                        "top" => ly >= 0.0 && ly <= 3.0 && along_x,
                        "bottom" => ly < wh && ly >= wh - 3.0 && along_x,
                        "left" => lx >= 0.0 && lx <= 3.0 && along_y,
                        "right" => lx < ww && lx >= ww - 3.0 && along_y,
                        _ => false,
                    };
                }
            }
            let ignore = !inside;
            if ignoring != Some(ignore) {
                set_click_through(&w, ignore);
                ignoring = Some(ignore);
            }
            if inside != hovered {
                hovered = inside;
                let _ = app.emit("island_hover", inside);
            }
        }
    });
}

/// Open or closed, as the page last said. The window no longer follows it (it keeps one size per
/// edge); only the island inside changes shape.
#[tauri::command]
fn notch_expand(_app: AppHandle, on: bool) {
    EXPANDED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Live Activity (1), alert (2) or neither (0), as the page last said; kept for diagnostics only
#[tauri::command]
fn notch_peek(_app: AppHandle, kind: u8) {
    PEEK.store(kind.min(2), std::sync::atomic::Ordering::Relaxed);
}

/// Keeps a free-floating drag inside the combined bounds of every attached monitor (not just the
/// primary one), so a drag onto a second screen at a different DPI still lands somewhere visible.
#[cfg(windows)]
fn clamp_to_virtual_screen(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let cx = x.clamp(vx, (vx + vw - w).max(vx));
        let cy = y.clamp(vy, (vy + vh - h).max(vy));
        (cx, cy)
    }
}
#[cfg(not(windows))]
fn clamp_to_virtual_screen(x: i32, y: i32, _w: i32, _h: i32) -> (i32, i32) {
    (x, y)
}

/// Older entry point name still used by tray.rs
pub fn reset_bar(app: &AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.notch_y = 0.5;
        c.bar_x = None;
        c.bar_y = None;
        c.edge = "right".into();
        c.monitor = None; // back to the primary monitor
        config::save(&c);
    }
    place_notch(app);
    emit_config(app);
}

/// Click-through on or off. Only WS_EX_TRANSPARENT is toggled; WS_EX_LAYERED is set once and kept.
/// Tauri's set_ignore_cursor_events adds and removes both, and every time WS_EX_LAYERED is dropped
/// Windows re-composites the window — the island blinked whenever the cursor crossed its edge.
#[cfg(windows)]
fn set_click_through(w: &tauri::WebviewWindow, on: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TRANSPARENT};
    if let Ok(h) = w.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let mut next = ex | WS_EX_LAYERED.0 as isize;
            if on {
                next |= WS_EX_TRANSPARENT.0 as isize;
            } else {
                next &= !(WS_EX_TRANSPARENT.0 as isize);
            }
            if next != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
            }
        }
    }
}
#[cfg(not(windows))]
fn set_click_through(w: &tauri::WebviewWindow, on: bool) {
    let _ = w.set_ignore_cursor_events(on);
}

/// Drag, the iOS AssistiveTouch gesture. The page calls this once a press on the handle or pill moves
/// more than 4 px; from then on a Rust thread follows the system cursor (WebView mousemove is
/// unreliable once the window itself starts moving), keeping the button centred under it. On release
/// the button slides to the nearest edge (or stays where it was let go in island mode) and the
/// handle takes its place.
static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
fn left_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
fn left_button_down() -> bool {
    false
}

#[tauri::command]
fn drag_begin(app: AppHandle) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    if DRAGGING.swap(true, Ordering::SeqCst) {
        return;
    }
    let (free_move, notch_scale) = {
        let st = app.state::<AppState>();
        let c = st.cfg.lock().unwrap();
        (c.drag_enabled, c.scale.clamp(0.7, 1.6))
    };
    std::thread::spawn(move || {
        let bail = |app: &AppHandle| {
            DRAGGING.store(false, Ordering::SeqCst);
            let _ = app.emit("drag_end", false);
        };
        let Some(w) = app.get_webview_window("notch") else { return bail(&app) };
        let (Ok(cur), Ok(wpos)) = (app.cursor_position(), w.outer_position()) else { return bail(&app) };
        // The window keeps its size and simply travels with the cursor. Shrinking a transparent
        // WebView window to a pill at the start of a drag (and growing it back at the end) flashed
        // blank frames; the page now draws the pill right where it was pressed instead.
        let off = (cur.x - wpos.x as f64, cur.y - wpos.y as f64);
        EXPANDED.store(false, Ordering::Relaxed);
        *HOT.lock().unwrap() = None;
        set_click_through(&w, false);
        let mut last = (wpos.x, wpos.y);
        loop {
            if !left_button_down() {
                break;
            }
            if let Ok(c) = app.cursor_position() {
                let p = ((c.x - off.0).round() as i32, (c.y - off.1).round() as i32);
                if p != last {
                    last = p;
                    let _ = w.set_position(tauri::PhysicalPosition::new(p.0, p.1));
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }

        // Where the pill is now: the press point, carried along with the window
        let centre = (last.0 + off.0.round() as i32, last.1 + off.1.round() as i32);
        // The screen it was let go on — any monitor, not just the primary one
        let Some(mon) = monitor_at(&app, centre.0, centre.1) else { return bail(&app) };
        let mon_name = mon.name().cloned();
        let ms = mon.scale_factor();
        if free_move {
            // Floating, the saved position is the closed pill's centre — exactly where it was let go
            {
                let st = app.state::<AppState>();
                let mut c = st.cfg.lock().unwrap();
                c.bar_x = Some(centre.0);
                c.bar_y = Some(centre.1);
                c.monitor = mon_name.clone();
                config::save(&c);
            }
            applog(&format!("notch drag (island): centre=({},{}) monitor={mon_name:?}", centre.0, centre.1));
        } else {
            let (mx, my) = (mon.position().x, mon.position().y);
            let (mw, mh) = (mon.size().width as i32, mon.size().height as i32);
            // Snap to the nearest edge, right where it was let go along that edge
            let gaps = [
                ("left", centre.0 - mx),
                ("right", mx + mw - centre.0),
                ("top", centre.1 - my),
                ("bottom", my + mh - centre.1),
            ];
            let (edge, _) = gaps.iter().min_by_key(|(_, g)| *g).copied().unwrap_or(("right", 0));
            let horizontal = edge == "top" || edge == "bottom";
            // The closed island's centre on that edge, level with where it was let go
            let (pw, ph) = if horizontal { PILL_WIDE } else { PILL_TALL };
            let (pw, ph) = ((pw * notch_scale * ms).round() as i32, (ph * notch_scale * ms).round() as i32);
            let tx = centre.0.clamp(mx + pw / 2, (mx + mw - pw / 2).max(mx + pw / 2));
            let ty = centre.1.clamp(my + ph / 2, (my + mh - ph / 2).max(my + ph / 2));
            let dock = match edge {
                "top" => (tx, my + ph / 2),
                "bottom" => (tx, my + mh - ph / 2),
                "left" => (mx + pw / 2, ty),
                _ => (mx + mw - pw / 2, ty),
            };
            let ratio = if horizontal {
                (dock.0 - mx) as f64 / mw.max(1) as f64
            } else {
                (dock.1 - my) as f64 / mh.max(1) as f64
            };
            {
                let st = app.state::<AppState>();
                let mut c = st.cfg.lock().unwrap();
                c.edge = edge.to_string();
                c.notch_y = ratio.clamp(0.0, 1.0);
                c.monitor = mon_name.clone();
                config::save(&c);
            }
            applog(&format!("notch drag: snapped to {edge} ratio={ratio:.3} monitor={mon_name:?}"));
            // Glide the pill onto that spot and let it settle with a small overshoot — a spring's
            // last bounce — rather than stopping dead
            let target = (dock.0 - off.0.round() as i32, dock.1 - off.1.round() as i32);
            const STEPS: i32 = 22;
            for i in 1..=STEPS {
                let t = i as f64 / STEPS as f64;
                // easeOutBack, gentle: overshoots a few percent and comes back
                let (c1, c3) = (1.2, 2.2);
                let e = 1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2);
                let x = last.0 as f64 + (target.0 - last.0) as f64 * e;
                let y = last.1 as f64 + (target.1 - last.1) as f64 * e;
                let _ = w.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
                std::thread::sleep(Duration::from_millis(12));
            }
        }
        // Land: the page hides the pill at once, the window takes its docked place, and the island
        // fades in there — no frame with the pill or the island in the wrong spot
        let _ = app.emit("drag_land", true);
        std::thread::sleep(Duration::from_millis(40));
        place_notch(&app);
        emit_config(&app);
        DRAGGING.store(false, Ordering::SeqCst);
        let _ = app.emit("drag_end", true);
    });
}
pub fn place_bar(app: &AppHandle) {
    place_notch(app);
}
pub fn toggle_drag(app: &AppHandle) {
    // The notch stays welded to the edge; kept as a no-op for the tray menu code path
    let _ = app;
}

pub fn apply_lang(app: &AppHandle, lang: &str) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.lang = lang.to_string();
        config::save(&c);
    }
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = tray::build_menu(app, lang) {
            let _ = tray.set_menu(Some(menu));
        }
    }
    broadcast(app);
}

/// The notch must never take focus: WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW
#[cfg(windows)]
fn noactivate(app: &AppHandle) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };
    if let Some(w) = app.get_webview_window("notch") {
        if let Ok(h) = w.hwnd() {
            unsafe {
                let hwnd =
                    windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
                );
            }
        }
    }
}
#[cfg(not(windows))]
fn noactivate(_app: &AppHandle) {}

/// Is a full-screen app (game, video, F11 browser) or presentation in front of the notch's monitor?
/// Returns what it found, for the log. Windows' own QUNS_BUSY was used alone at first and proved far
/// too broad: it also fires for a maximized window when the taskbar auto-hides, for invisible
/// full-screen helper windows (wallpaper engines, overlays) and for full screen on another monitor —
/// the notch vanished with no game in sight. Now only Direct3D exclusive full screen and
/// presentation mode are taken from Windows; anything else must be the foreground window itself
/// covering the whole primary monitor without being merely maximized.
#[cfg(windows)]
fn fullscreen_busy(notch: isize) -> Option<String> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed,
    };
    unsafe {
        match SHQueryUserNotificationState() {
            Ok(s) if s == QUNS_RUNNING_D3D_FULL_SCREEN => return Some("Direct3D full screen".into()),
            Ok(s) if s == QUNS_PRESENTATION_MODE => return Some("presentation mode".into()),
            _ => {}
        }
        let fg = GetForegroundWindow();
        if fg.0.is_null() || !IsWindowVisible(fg).as_bool() || IsIconic(fg).as_bool() || IsZoomed(fg).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if pid == std::process::id() {
            return None;
        }
        let mut buf = [0u16; 128];
        let n = GetClassNameW(fg, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        // The desktop and the taskbar cover the screen too, and are never "an app in full screen"
        if ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"].contains(&class.as_str()) {
            return None;
        }
        // Only the monitor the notch itself is on — full screen on the other screen leaves it alone
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if mon != MonitorFromWindow(HWND(notch as *mut core::ffi::c_void), MONITOR_DEFAULTTOPRIMARY) {
            return None;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut r = RECT::default();
        if !GetMonitorInfoW(mon, &mut mi).as_bool() || GetWindowRect(fg, &mut r).is_err() {
            return None;
        }
        let m = mi.rcMonitor;
        let covers = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
        covers.then(|| {
            let exe = crate::focus::proc_maps().name.get(&pid).cloned().unwrap_or_default();
            format!("{exe} ({class}) covers the screen")
        })
    }
}
#[cfg(not(windows))]
fn fullscreen_busy(_notch: isize) -> Option<String> {
    None
}

/// The notch window's handle as a plain number, for the Win32 checks above (0 when unavailable)
#[cfg(windows)]
fn notch_hwnd(app: &AppHandle) -> isize {
    app.get_webview_window("notch").and_then(|w| w.hwnd().ok()).map(|h| h.0 as isize).unwrap_or(0)
}
#[cfg(not(windows))]
fn notch_hwnd(_app: &AppHandle) -> isize {
    0
}

/// Shows the notch without activating it: a plain show() may take focus from the app in front
#[cfg(windows)]
fn set_shown(w: &tauri::WebviewWindow, on: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE};
    if let Ok(h) = w.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
        unsafe {
            let _ = ShowWindow(hwnd, if on { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}
#[cfg(not(windows))]
fn set_shown(w: &tauri::WebviewWindow, on: bool) {
    let _ = if on { w.show() } else { w.hide() };
}

/// Keeps the notch visible and on top of the always-on-top band. Design apps (Photoshop's floating
/// panels, colour pickers, capture tools) put their own topmost windows up and the notch ended up
/// underneath them: still there, just covered — "it disappears by itself". Returns what had to be
/// repaired, for the log; the re-raise itself happens every time, since a window can be covered
/// while still holding WS_EX_TOPMOST.
#[cfg(windows)]
fn keep_on_top(w: &tauri::WebviewWindow) -> Option<&'static str> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, IsWindowVisible, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSENDCHANGING, SWP_NOSIZE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    };
    let h = w.hwnd().ok()?;
    let hwnd = windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
    unsafe {
        let repaired = if !IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Some("was hidden")
        } else if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize == 0 {
            Some("had lost always-on-top")
        } else {
            None
        };
        // As Bloom does: raise without activating or notifying, then re-stamp NOACTIVATE +
        // TOOLWINDOW, which some Windows builds strip when a window is made topmost again
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOSENDCHANGING);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize;
        if want != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        }
        repaired
    }
}
#[cfg(not(windows))]
fn keep_on_top(_w: &tauri::WebviewWindow) -> Option<&'static str> {
    None
}

/// Gets out of the way of full-screen apps (tray: Hide during full-screen apps), checked once a second;
/// the rest of the time it keeps the notch on top (see keep_on_top)
fn start_fullscreen_watch(app: AppHandle) {
    std::thread::spawn(move || {
        let mut hidden = false;
        let mut streak = 0u8;
        let mut tick = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            tick = tick.wrapping_add(1);
            if !hidden && tick % 2 == 0 && !DRAGGING.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(w) = app.get_webview_window("notch") {
                    if let Some(what) = keep_on_top(&w) {
                        applog(&format!("keep-on-top: notch {what} — restored"));
                    }
                }
            }
            let enabled = app.state::<AppState>().cfg.lock().map(|c| c.hide_fullscreen).unwrap_or(false);
            let why = if enabled && !DRAGGING.load(std::sync::atomic::Ordering::SeqCst) { fullscreen_busy(notch_hwnd(&app)) } else { None };
            // Two readings in a row before hiding, so a window that covers the screen for a moment
            // (a splash screen, a transition) does not blink the notch away; shown again at once.
            streak = if why.is_some() { streak.saturating_add(1) } else { 0 };
            let hide = streak >= 2;
            if hide == hidden {
                continue;
            }
            hidden = hide;
            if let Some(w) = app.get_webview_window("notch") {
                set_shown(&w, !hide);
            }
            match why {
                Some(reason) if hide => applog(&format!("full-screen watch: notch hidden — {reason}")),
                _ => applog("full-screen watch: notch shown"),
            }
        }
    });
}

// ---------------- commands ----------------

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> state::Snapshot {
    let store = state.store.lock().unwrap();
    let cfg = state.cfg.lock().unwrap();
    store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
}

#[tauri::command]
fn get_usage(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.usage.lock().unwrap().clone()
}

#[tauri::command]
fn refresh_usage(app: AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut u = st.usage.lock().unwrap();
        u.backoff_until = 0;
    }
    usage::request_refresh();
    codex::request_refresh();
    cursor::request_refresh();
    antigravity::request_refresh();
    commandcode::request_refresh();
    router9::request_refresh();
    deepseek::request_refresh();
}

#[tauri::command]
fn get_deepseek(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.deepseek.lock().unwrap().clone()
}

#[tauri::command]
fn get_antigravity(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.antigravity.lock().unwrap().clone()
}

#[tauri::command]
fn get_commandcode(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.commandcode.lock().unwrap().clone()
}

#[tauri::command]
fn get_router9(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.router9.lock().unwrap().clone()
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> config::Config {
    state.cfg.lock().unwrap().clone()
}

/// Pushes the current config to the page (opacity/scale live-applied via CSS) and repositions/resizes the window
pub fn emit_config(app: &AppHandle) {
    let cfg = {
        let st = app.state::<AppState>();
        let c = st.cfg.lock().unwrap();
        c.clone()
    };
    let _ = app.emit("config", &cfg);
}

#[tauri::command]
fn get_activity(state: tauri::State<AppState>) -> Vec<activity::Activity> {
    state.activity.lock().unwrap().clone()
}

#[tauri::command]
fn get_glyphs(state: tauri::State<AppState>) -> std::collections::HashMap<String, glyphs::Glyph> {
    state.glyphs.lock().unwrap().clone()
}

/// Collects the glyphs again and pushes them to the page (tray refresh, or the user just dropped in an override)
pub fn reload_glyphs(app: &AppHandle) {
    let m = glyphs::collect();
    let st = app.state::<AppState>();
    *st.glyphs.lock().unwrap() = m.clone();
    let _ = app.emit("glyphs", &m);
}

#[tauri::command]
fn open_data_dir() {
    let dir = config::config_path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let _ = std::fs::create_dir_all(glyphs::user_dir());
    let mut cmd = std::process::Command::new("explorer");
    cmd.arg(dir.as_os_str());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

#[tauri::command]
fn get_cursor(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.cursor.lock().unwrap().clone()
}

#[tauri::command]
fn get_codex(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.codex.lock().unwrap().clone()
}

/// A click on a cell opens that provider's usage page
#[tauri::command]
fn open_provider_page(provider: String) {
    let url = match provider.as_str() {
        "codex" => "https://chatgpt.com/#settings/Account",
        "cursor" => "https://cursor.com/dashboard",
        "gemini" => "https://antigravity.google",
        "commandcode" => "https://commandcode.ai",
        "router9" => "http://127.0.0.1:20128/dashboard",
        "deepseek" => "https://platform.deepseek.com/usage",
        _ => "https://claude.ai/settings/usage",
    };
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", url]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

/// Card expansion state: Some(hot rectangles, in **physical pixels** relative to the window's
/// top-left as x,y,w,h) = expanded; None = collapsed. The page converts the rectangles with its
/// own devicePixelRatio before reporting them, so no scale conversion happens on this side —
/// WebView2's DPR and the window's scale_factor can disagree (see report_dpr).
static HOT: Mutex<Option<Vec<[f64; 4]>>> = Mutex::new(None);

#[tauri::command]
fn set_expanded(on: bool, rects: Option<Vec<[f64; 4]>>) {
    *HOT.lock().unwrap() = if on { Some(rects.unwrap_or_default()) } else { None };
}

/// The WebView zoom currently applied (1.0 = uncorrected)
static ZOOM: Mutex<f64> = Mutex::new(1.0);

pub fn applog(line: &str) {
    use std::io::Write;
    let log = config::config_path().with_file_name("run.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// Root cause: with two monitors (150 % / 200 %) WebView2 picked a devicePixelRatio of 2.0 while
/// the window was sized for the primary monitor's 1.5, so the page was 255 CSS px wide instead of
/// the designed 340 and every coordinate conversion was off (the watchdog misfired and the card
/// flashed away). Fix: the page reports its DPR, and when it differs from the primary monitor's
/// scale, set_zoom pulls the effective DPR back to that scale, restoring the 340 px width.
#[tauri::command]
fn report_dpr(app: AppHandle, dpr: f64, w: f64, h: f64) {
    let Some(win) = app.get_webview_window("notch") else { return };
    // The scale of the monitor the notch lives on, which is no longer always the primary one
    let want = notch_monitor(&app)
        .map(|m| m.scale_factor())
        .unwrap_or_else(|| win.scale_factor().unwrap_or(1.0));
    let mut z = ZOOM.lock().unwrap();
    let base = if *z > 0.0 { dpr / *z } else { dpr };
    let target = if base > 0.0 { want / base } else { 1.0 };
    applog(&format!(
        "dpr report: dpr={dpr:.3} viewport={w:.0}x{h:.0} monitor_scale={want:.3} zoom_applied={:.3} -> target_zoom={target:.3}",
        *z
    ));
    // Oscillation guard: at most three corrections per process (if the DPR does not follow the zoom, stop chasing it)
    static APPLIED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if (dpr - want).abs() > 0.02
        && (target - *z).abs() > 0.01
        && (0.25..=4.0).contains(&target)
        && APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
    {
        match win.set_zoom(target) {
            Ok(()) => {
                *z = target;
                applog(&format!("dpr correction: set_zoom({target:.3}) ok"));
            }
            Err(e) => applog(&format!("dpr correction failed: {e}")),
        }
    }
}

/// WebView2's mouseleave is unreliable inside a NOACTIVATE transparent window — a cursor that
/// leaves quickly often produces no WM_MOUSELEAVE, and the card stays up. Rather than trust DOM
/// events, the Rust side watches the system cursor while the card is expanded and emits
/// pointer_left once the cursor is outside; the page collapses after its 250 ms grace period.
/// "Outside the window" is not the test, though: the window has a 340×460 transparent area, so
/// the cursor is compared against the hot rectangles the page reports (pill, card, and the gap
/// between them), and two consecutive misses (300 ms) count as leaving.
fn start_pointer_watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        let mut miss = 0u8;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let rects = match HOT.lock().unwrap().clone() {
                Some(r) => r,
                None => {
                    miss = 0;
                    continue;
                }
            };
            let Some(w) = app.get_webview_window("notch") else { continue };
            let (Ok(pos), Ok(cur)) = (w.outer_position(), app.cursor_position()) else { continue };
            // Cursor position relative to the window's top-left, in physical pixels; the hot rectangles are physical too, so no scale conversion
            let lx = cur.x - pos.x as f64;
            let ly = cur.y - pos.y as f64;
            const PAD: f64 = 10.0;
            let in_window = w
                .outer_size()
                .map(|s| lx >= 0.0 && ly >= 0.0 && lx < s.width as f64 && ly < s.height as f64)
                .unwrap_or(true);
            let mut inside = in_window && rects.iter().any(|r| {
                lx >= r[0] - PAD && ly >= r[1] - PAD && lx < r[0] + r[2] + PAD && ly < r[1] + r[3] + PAD
            });
            // The gap between hot rectangles (pill and card) counts as inside: use the bounding box of all of them
            if !inside && in_window && rects.len() > 1 {
                let x0 = rects.iter().map(|r| r[0]).fold(f64::MAX, f64::min);
                let y0 = rects.iter().map(|r| r[1]).fold(f64::MAX, f64::min);
                let x1 = rects.iter().map(|r| r[0] + r[2]).fold(f64::MIN, f64::max);
                let y1 = rects.iter().map(|r| r[1] + r[3]).fold(f64::MIN, f64::max);
                inside = lx >= x0 && ly >= y0 && lx < x1 && ly < y1;
            }
            static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
                applog(&format!(
                    "watchdog: cursor_rel=({lx:.0},{ly:.0}) inside={inside} rects={rects:?} winpos=({},{})",
                    pos.x, pos.y
                ));
            }
            if inside {
                miss = 0;
            } else {
                miss += 1;
                if miss >= 2 {
                    miss = 0;
                    *HOT.lock().unwrap() = None;
                    let _ = app.emit("pointer_left", ());
                }
            }
        }
    });
}

/// Log channel for the page: JS writes key diagnostics into run.log (if invoke itself fails, the page reports on screen instead)
#[tauri::command]
fn log_js(msg: String) {
    applog(&format!("js: {}", msg.chars().take(600).collect::<String>()));
}

#[tauri::command]
fn open_usage_page() {
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", "https://claude.ai/settings/usage"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}

#[tauri::command]
fn focus_session(app: AppHandle, id: String) -> bool {
    let ppid = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.ppid_of(&id)
    };
    // A session with no terminal of its own (Claude Desktop's Code tab, or the terminal was closed)
    // lands on Claude Desktop instead of "not found"
    match ppid {
        Some(p) => focus::focus_terminal(p) || focus::focus_claude_desktop(),
        None => focus::focus_claude_desktop(),
    }
}

#[tauri::command]
fn dismiss_session(app: AppHandle, id: String) {
    {
        let st = app.state::<AppState>();
        let mut store = st.store.lock().unwrap();
        store.dismiss(&id);
    }
    broadcast(&app);
}

#[tauri::command]
fn set_lang(app: AppHandle, lang: String) {
    apply_lang(&app, &lang);
}

/// Seen-clears-it: looking at a session acknowledges it (engine behaviour, unchanged)
#[cfg(windows)]
fn ack_scan(app: &AppHandle) -> bool {
    let need = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.has_done()
    };
    if !need {
        return false;
    }
    let fg = focus::fg_pid();
    if fg == 0 {
        return false;
    }
    let maps = focus::proc_maps();
    let fg_name = maps.name.get(&fg).cloned().unwrap_or_default();
    let fg_is_claude_desktop = fg_name.contains("claude") && !fg_name.contains("codenotch");
    let st = app.state::<AppState>();
    let mut store = st.store.lock().unwrap();
    store.ack_done(|s| {
        if s.ppid == 0 {
            fg_is_claude_desktop
        } else {
            focus::pid_hits_chain(fg, &focus::chain_of(s.ppid, &maps.ppid), &maps)
        }
    })
}
#[cfg(not(windows))]
fn ack_scan(_app: &AppHandle) -> bool {
    false
}

// ---------------- main ----------------

#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
#[cfg(not(windows))]
fn attach_console() {}

fn report(r: Result<String, String>) {
    let msg = match r {
        Ok(m) => format!("OK: {m}"),
        Err(e) => format!("FAILED: {e}"),
    };
    println!("{msg}");
    let log = config::config_path().with_file_name("install.log");
    let _ = std::fs::write(log, &msg);
}

fn main() {
    attach_console();
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1) {
        match cmd.as_str() {
            "install-hooks" => {
                report(hooks_install::install());
                return;
            }
            "uninstall-hooks" => {
                report(hooks_install::uninstall());
                return;
            }
            "autostart" => {
                let r = match args.get(2).map(|s| s.as_str()) {
                    Some("on") => autostart::enable(),
                    Some("off") => autostart::disable(),
                    _ => Err("usage: codenotch.exe autostart on|off".into()),
                };
                report(r);
                return;
            }
            "doctor" => {
                let out = if args.get(2).map(|s| s.as_str()) == Some("deep") { diag::run() } else { doctor::run() };
                println!("{out}");
                let log = config::config_path().with_file_name("doctor.log");
                let _ = std::fs::write(log, &out);
                return;
            }
            _ => {}
        }
    }

    let cfg = config::load();
    let port = cfg.port;

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Launching a freshly built exe while the old one is still running lands here: the new
            // instance is turned away and what stays on screen is the old process. Say so loudly.
            applog(&format!("single instance: another launch was refused; the running instance is build={BUILD} — quit it from the tray first if you just rebuilt"));
            let _ = app.emit("notice", format!("Codenotch is already running ({BUILD}) — quit it from the tray before starting a new build"));
        }))
        .manage(AppState {
            store: Mutex::new(Default::default()),
            cfg: Mutex::new(cfg),
            usage: Mutex::new(usage::load_persisted()),
            codex: Mutex::new(codex::load_persisted()),
            cursor: Mutex::new(cursor::load_persisted()),
            antigravity: Mutex::new(antigravity::load_persisted()),
            commandcode: Mutex::new(commandcode::load_persisted()),
            router9: Mutex::new(router9::load_persisted()),
            deepseek: Mutex::new(deepseek::load_persisted()),
            glyphs: Mutex::new(Default::default()),
            activity: Mutex::new(Vec::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_usage,
            get_codex,
            get_cursor,
            get_antigravity,
            get_commandcode,
            get_router9,
            get_deepseek,
            get_placement,
            update_island_rect,
            get_config,
            notch_expand,
            notch_peek,
            save_layout,
            set_pref,
            settings::save_deepseek_key,
            settings::remove_deepseek_key,
            settings::open_settings,
            settings::settings_status,
            settings::save_commandcode_key,
            settings::remove_commandcode_key,
            settings::save_router9,
            settings::clear_router9,
            settings::router9_local_token,
            settings::router9_connections,
            settings::router9_set_active,
            settings::open_link,
            get_glyphs,
            get_activity,
            open_data_dir,
            drag_begin,
            open_provider_page,
            refresh_usage,
            open_usage_page,
            set_expanded,
            report_dpr,
            log_js,
            focus_session,
            dismiss_session,
            set_lang
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            place_notch(&handle);
            noactivate(&handle);
            if let Some(w) = handle.get_webview_window("notch") {
                let _ = w.show();
            }
            emit_config(&handle);
            tray::setup(&handle)?;
            server::start(handle.clone(), port);
            watcher::start(handle.clone());
            usage::start(handle.clone());
            codex::start(handle.clone());
            cursor::start(handle.clone());
            antigravity::start(handle.clone());
            commandcode::start(handle.clone());
            router9::start(handle.clone());
            deepseek::start(handle.clone());
            activity::start(handle.clone());
            // Collecting glyphs may read icon resources out of a few executables; do it off the main thread and push when done
            let gh = handle.clone();
            std::thread::spawn(move || reload_glyphs(&gh));
            start_pointer_watchdog(handle.clone());
            start_fullscreen_watch(handle.clone());
            start_hit_test(handle.clone());
            // Seen-clears-it scan
            let acker = handle.clone();
            std::thread::spawn(move || {
                activity::lower_thread_priority();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if ack_scan(&acker) {
                        broadcast(&acker);
                    }
                }
            });
            // Stale session cleanup
            let sweeper = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let changed = {
                    let st = sweeper.state::<AppState>();
                    let mut s = st.store.lock().unwrap();
                    s.sweep()
                };
                if changed {
                    broadcast(&sweeper);
                }
            });
            // Persist the config (codenotch-hook reads the port from it)
            {
                let st = handle.state::<AppState>();
                let c = st.cfg.lock().unwrap();
                config::save(&c);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Codenotch failed to start");
}
