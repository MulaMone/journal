#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{webview::NewWindowResponse, WebviewUrl, WebviewWindowBuilder};

static LINK_COUNT: AtomicUsize = AtomicUsize::new(0);

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            let handle = app.handle().clone();
            let cfg = app.config().app.windows[0].clone();
            WebviewWindowBuilder::from_config(app.handle(), &cfg)?
                // target="_blank" links and window.open() open in their own EDGE window
                .on_new_window(move |url, _features| {
                    let h = handle.clone();
                    std::thread::spawn(move || {
                        let n = LINK_COUNT.fetch_add(1, Ordering::SeqCst);
                        let _ = WebviewWindowBuilder::new(
                            &h,
                            format!("link-{n}"),
                            WebviewUrl::External(url),
                        )
                        .title("EDGE — Link")
                        .inner_size(1200.0, 850.0)
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
