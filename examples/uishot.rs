// Dev tool: opens the real Bindle window, waits for it to draw, renders it to a
// PNG (UISHOT_OUT, default uishot.png) and exits. Usage:
//   UISHOT_OUT=/tmp/shot.png cargo run --example uishot -- sample.md

use std::time::Duration;

use adw::prelude::*;
use gtk4::{gio, glib};
use bindle::ui;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        // Separate id so it never joins a running Bindle instance.
        .application_id("no.oyzmo.Bindle.Uishot")
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|_| {
        ui::fix_font_rendering();
        ui::load_css();
    });
    app.connect_activate(|app| shoot(app, None));
    app.connect_open(|app, files, _| {
        shoot(app, files.first().and_then(|f| f.path()));
    });
    app.run()
}

fn env_ms(name: &str, fallback: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

fn shoot(app: &adw::Application, path: Option<std::path::PathBuf>) {
    ui::present(app, path);
    // UISHOT_ACTIONS=win.view-mode,win.show-insert activates window actions
    // before the shot (e.g. render in view mode or with panels toggled).
    // name=value passes a string parameter (e.g. win.open-recent=/a/b.md).
    // UISHOT_ACTION_DELAY=ms waits first, for actions that need the opened
    // document to be there — opening a file is asynchronous, so at 0 ms the
    // buffer is still empty.
    if let Ok(actions) = std::env::var("UISHOT_ACTIONS") {
        let app = app.clone();
        let delay = env_ms("UISHOT_ACTION_DELAY", 0);
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(window) = app.active_window() {
                for spec in actions.split(',').filter(|s| !s.is_empty()) {
                    let (name, param) = match spec.split_once('=') {
                        Some((n, p)) => (n, Some(p.to_variant())),
                        None => (spec, None),
                    };
                    let _ = WidgetExt::activate_action(&window, name, param.as_ref());
                }
            }
        });
    }
    // UISHOT_RESIZE=WxH resizes the window after UISHOT_ACTION_DELAY — for
    // checking that layout follows a live resize, not just the first size.
    if let Some((w, h)) = std::env::var("UISHOT_RESIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?))
    }) {
        let app = app.clone();
        let delay = env_ms("UISHOT_ACTION_DELAY", 0);
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(window) = app.active_window() {
                window.set_default_size(w, h);
            }
        });
    }
    // UISHOT_FIND=word types a query into the find bar (combine with
    // UISHOT_ACTIONS=win.find, which focuses the entry).
    if let Ok(query) = std::env::var("UISHOT_FIND") {
        if let Some(entry) = app
            .active_window()
            .and_then(|w| GtkWindowExt::focus(&w))
            .and_then(|w| w.ancestor(gtk4::SearchEntry::static_type()))
            .and_then(|w| w.downcast::<gtk4::SearchEntry>().ok())
        {
            entry.set_text(&query);
        }
    }
    // UISHOT_SCROLL=0.5 scrolls the focused view (the editor, normally)
    // that far down its document after the action delay — for checking
    // what follows editor scrolling.
    if let Some(frac) = std::env::var("UISHOT_SCROLL").ok().and_then(|v| v.parse::<f64>().ok()) {
        let app = app.clone();
        let delay = env_ms("UISHOT_ACTION_DELAY", 0) + 200;
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(scroll) = app
                .active_window()
                .and_then(|w| GtkWindowExt::focus(&w))
                .and_then(|w| w.ancestor(gtk4::ScrolledWindow::static_type()))
                .and_then(|w| w.downcast::<gtk4::ScrolledWindow>().ok())
            {
                let adj = scroll.vadjustment();
                adj.set_value(adj.lower() + frac * (adj.upper() - adj.lower() - adj.page_size()));
            }
        });
    }
    let app = app.clone();
    // The shot waits out the action delay on top of its own, so raising one
    // never silently cuts off the other.
    let wait = env_ms("UISHOT_WAIT", 800) + env_ms("UISHOT_ACTION_DELAY", 0);
    glib::timeout_add_local_once(Duration::from_millis(wait), move || {
        let window = app.active_window().expect("no window");
        let width = window.width();
        let height = window.height();
        let paintable = gtk4::WidgetPaintable::new(Some(&window));
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(&snapshot, width as f64, height as f64);
        let out = std::env::var("UISHOT_OUT").unwrap_or_else(|_| "uishot.png".into());
        match snapshot.to_node() {
            Some(node) => {
                let renderer = window.renderer().expect("window has no renderer");
                let texture = renderer.render_texture(
                    &node,
                    Some(&gtk4::graphene::Rect::new(
                        0.0,
                        0.0,
                        width as f32,
                        height as f32,
                    )),
                );
                texture.save_to_png(&out).expect("could not save png");
                eprintln!("saved {out} ({width}x{height})");
            }
            None => eprintln!("empty snapshot, nothing saved"),
        }
        app.quit();
    });
}
