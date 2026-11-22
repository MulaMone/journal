#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{webview::NewWindowResponse, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

static CHILD_COUNT: AtomicUsize = AtomicUsize::new(0);
const DESKTOP_FLAG: &str = "window.__EDGE_DESKTOP = true; window.__EDGE_TB = true;";

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // closing the main EDGE window closes every child window and exits
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let WindowEvent::Destroyed = event {
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
                        let _ = WebviewWindowBuilder::new(
                            &h,
                            format!("child-{n}"),
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
