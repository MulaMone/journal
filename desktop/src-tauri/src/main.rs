#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tauri::{webview::NewWindowResponse, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_window_state::StateFlags;

static CHILD_COUNT: AtomicUsize = AtomicUsize::new(0);
/// GPU chart processes started by this app; they are closed when the main EDGE window closes.
static CHARTS: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

fn close_native_charts() {
    if let Ok(mut v) = CHARTS.lock() {
        for c in v.iter_mut() {
            let _ = c.kill();
        }
        v.clear();
    }
}
const DESKTOP_FLAG: &str = "window.__EDGE_DESKTOP = true; window.__EDGE_TB = true;";

/// Only letters/digits, so nothing odd can reach the child's command line.
fn clean(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect()
}

/// The GPU chart button opens ?native=chart&sym=..&tf=..#t=<token>. Start edge-chart.exe (next to this exe),
/// hand it the saved OANDA token through its environment, and report whether it started.
/// The token only travels inside this app: the URL is never navigated to, and the fragment is never sent anywhere.
fn launch_native_chart(app: &tauri::AppHandle, url: &Url) -> bool {
    let (mut sym, mut tf) = ("NQ".to_string(), "5m".to_string());
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "sym" => sym = clean(&v),
            "tf" => tf = clean(&v),
            _ => {}
        }
    }
    let tok = url.fragment().and_then(|f| f.strip_prefix("t=")).unwrap_or("").to_string();
    let exe = match std::env::current_exe() {
        Ok(p) => p.with_file_name("edge-chart.exe"),
        Err(_) => return false,
    };
    if !exe.exists() {
        return false;
    }
    // open the GPU chart right next to the EDGE window: same top, same height, same width, on its right side
    let mut geo: Vec<String> = vec![];
    if let Some(w) = app.get_webview_window("main") {
        if let (Ok(p), Ok(sz), Ok(sf)) = (w.outer_position(), w.outer_size(), w.scale_factor()) {
            let sf = if sf > 0.0 { sf } else { 1.0 };
            let (mut x, y) = ((p.x as f64 + sz.width as f64) / sf, p.y as f64 / sf);
            let (cw, ch) = (sz.width as f64 / sf, sz.height as f64 / sf);
            // no room to the right of EDGE on this monitor: tuck the chart against the monitor's right edge instead
            if let Ok(Some(m)) = w.current_monitor() {
                let right = (m.position().x as f64 + m.size().width as f64) / sf;
                if x + cw > right {
                    x = (right - cw).max(0.0);
                }
            }
            geo = vec!["--pos".into(), format!("{:.0},{:.0}", x, y), "--size".into(), format!("{:.0},{:.0}", cw, ch)];
        }
    }
    match std::process::Command::new(exe)
        .args(["--sym", &sym, "--tf", &tf])
        .args(&geo)
        .env("EDGE_OANDA_TOKEN", tok)
        .spawn()
    {
        Ok(child) => {
            if let Ok(mut v) = CHARTS.lock() {
                v.push(child);
            }
            true
        }
        Err(_) => false,
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_window_state::Builder::default()
                // never restore the old "native title bar on" setting
                .with_state_flags(StateFlags::all() - StateFlags::DECORATIONS)
                .build(),
        )
        // closing the main EDGE window closes every child window and exits
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let WindowEvent::Destroyed = event {
                    close_native_charts();
                    window.app_handle().exit(0);
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let cfg = app.config().app.windows[0].clone();
            WebviewWindowBuilder::from_config(app.handle(), &cfg)?
                .initialization_script(DESKTOP_FLAG)
                // target="_blank", window.open(), Detach and New chart open in their own EDGE window
                .on_new_window(move |url, _features| {
                    let h = handle.clone();
                    let mut url = url;
                    // GPU chart button: start the native chart program. If it isn't installed next to this exe,
                    // fall back to the normal web chart window so the button is never dead.
                    if url.query().map_or(false, |q| q.contains("native=chart")) {
                        if launch_native_chart(&h, &url) {
                            return NewWindowResponse::Deny;
                        }
                        let q = url.query().unwrap_or("").replace("native=chart", "detached=1&tab=chart");
                        url.set_query(Some(&q));
                        url.set_fragment(None);
                    }
                    let (title, w, ht) = match url.query() {
                        Some(q) if q.contains("detached=1") && q.contains("tab=news") => {
                            ("EDGE — News", 760.0, 920.0)
                        }
                        Some(q) if q.contains("detached=1") && q.contains("tab=chart") => {
                            ("EDGE — Chart", 1300.0, 850.0)
                        }
                        _ => ("EDGE — Link", 1200.0, 850.0),
                    };
                    std::thread::spawn(move || {
                        let n = CHILD_COUNT.fetch_add(1, Ordering::SeqCst);
                        // Unique label every time: the window-state plugin remembers position by label, so a reused "child-0"
                        // could reopen off-screen (e.g. where the TV used to be) and look like it never opened.
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        let _ = WebviewWindowBuilder::new(
                            &h,
                            format!("child-{n}-{stamp}"),
                            WebviewUrl::External(url),
                        )
                        .title(title)
                        .decorations(false)
                        .inner_size(w, ht)
                        .initialization_script(DESKTOP_FLAG)
                        .build();
                    });
                    NewWindowResponse::Deny
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running EDGE");
}
