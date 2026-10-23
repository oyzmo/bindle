use adw::prelude::*;
use gtk4::{gio, glib};
use bindle::settings::Settings;
use bindle::ui;

const APP_ID: &str = "no.oyzmo.Bindle";

/// Append every panic, with a backtrace, to `<cache>/bindle/crash.log`, then
/// run the default hook. A panic inside a GTK callback aborts the process, and
/// an app launched from the desktop has no terminal for the message — without
/// this the only trace is a core dump that names the callback, not the bug.
fn install_crash_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let when = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format_iso8601().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let entry = format!(
            "--- Bindle {} panicked at {when}\n{info}\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        );
        let dir = glib::user_cache_dir().join("bindle");
        let _ = std::fs::create_dir_all(&dir).and_then(|_| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))?
                .write_all(entry.as_bytes())
        });
        default(info);
    }));
}

fn main() -> glib::ExitCode {
    install_crash_log();
    let mut flags = gio::ApplicationFlags::HANDLES_OPEN;
    // Uniqueness is decided at registration, so this only applies to
    // launches after the setting was toggled.
    if Settings::load().multiple_instances {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();

    app.connect_startup(|app| {
        ui::load_css();
        let accels: [(&str, &[&str]); 10] = [
            ("win.new", &["<Control>n"]),
            ("win.print", &["<Control>p"]),
            ("win.find", &["<Control>f"]),
            ("win.view-mode", &["<Control>e"]),
            ("win.open", &["<Control>o"]),
            ("win.save", &["<Control>s"]),
            ("win.save-as", &["<Control><Shift>s"]),
            // Ctrl+Space sits in the same place on every layout. Ctrl+/ needs
            // Shift on e.g. Nordic layouts where / is on 7, and Ctrl++ (main
            // row and keypad) stays for input methods that grab Ctrl+Space.
            ("win.insert", &["<Control>space", "<Control>slash", "<Control>plus", "<Control>KP_Add"]),
            ("win.bold", &["<Control>b"]),
            ("win.italic", &["<Control>i"]),
        ];
        for (action, keys) in accels {
            app.set_accels_for_action(action, keys);
        }
    });
    app.connect_activate(|app| ui::present(app, None));
    app.connect_open(|app, files, _| {
        let path = files.first().and_then(|f| f.path());
        ui::present(app, path);
    });

    app.run()
}
