// Dev tool: drives the real Bindle window through the Clean Up Assets flows
// (save-time suggestion, toast, dialog, trash) and checks the files on disk.
// Run via dev/cleansim/run.sh, which points XDG_* at a scratch dir so the
// user's settings and Trash are never touched. Prints PASS/FAIL per check
// and exits non-zero on any failure.

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{gio, glib};
use bindle::ui;

const STAMP: &str = "20261125-000001";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("no.oyzmo.Bindle.Cleansim")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| {
        ui::fix_font_rendering();
        ui::load_css();
    });
    app.connect_activate(|app| {
        ui::present(app, None);
        let app = app.clone();
        glib::spawn_future_local(async move {
            let failures = run(&app).await;
            eprintln!("\n{} failure(s)", failures);
            std::process::exit(if failures == 0 { 0 } else { 1 });
        });
    });
    app.run()
}

// --- fixtures ---

fn root() -> PathBuf {
    PathBuf::from(std::env::var("CLEANSIM_ROOT").expect("CLEANSIM_ROOT"))
}

fn trash_dir() -> PathBuf {
    glib::user_data_dir().join("Trash").join("files")
}

fn png() -> Vec<u8> {
    let pixbuf = gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, 8, 8).unwrap();
    pixbuf.fill(0x7fa07aff);
    pixbuf.save_to_bufferv("png", &[]).unwrap()
}

fn case_dir(name: &str) -> PathBuf {
    let dir = root().join(name);
    fs::create_dir_all(dir.join("assets")).unwrap();
    dir
}

/// A plain document `<stem>.md` in `dir` linking its own pasted image.
fn plain_doc(dir: &Path, stem: &str, extra: &str) -> (PathBuf, PathBuf, String) {
    let img = format!("{stem}-{STAMP}.png");
    fs::write(dir.join("assets").join(&img), png()).unwrap();
    let doc = dir.join(format!("{stem}.md"));
    fs::write(&doc, format!("# {stem}\n\nIntro.\n\n![pic](assets/{img})\n{extra}")).unwrap();
    (doc, dir.join("assets").join(&img), img)
}

// --- widget helpers ---

fn widgets(app: &adw::Application) -> Vec<gtk4::Widget> {
    fn walk(w: &gtk4::Widget, out: &mut Vec<gtk4::Widget>) {
        let mut child = w.first_child();
        while let Some(c) = child {
            out.push(c.clone());
            walk(&c, out);
            child = c.next_sibling();
        }
    }
    let mut out = Vec::new();
    if let Some(win) = app.active_window() {
        walk(win.upcast_ref(), &mut out);
    }
    out
}

fn labels(app: &adw::Application) -> Vec<String> {
    widgets(app)
        .into_iter()
        .filter(|w| w.is_mapped())
        .filter_map(|w| w.downcast::<gtk4::Label>().ok())
        .map(|l| l.label().to_string())
        .collect()
}

fn has_label(app: &adw::Application, part: &str) -> bool {
    labels(app).iter().any(|l| l.contains(part))
}

fn button(app: &adw::Application, text: &str) -> Option<gtk4::Button> {
    widgets(app)
        .into_iter()
        .filter(|w| w.is_mapped() && w.is_sensitive())
        .filter_map(|w| w.downcast::<gtk4::Button>().ok())
        .find(|b| b.label().is_some_and(|l| l.replace('_', "") == text))
}

fn checks(app: &adw::Application) -> Vec<bool> {
    widgets(app)
        .into_iter()
        .filter(|w| w.is_mapped())
        .filter_map(|w| w.downcast::<gtk4::CheckButton>().ok())
        .map(|c| c.is_active())
        .collect()
}

fn editor(app: &adw::Application) -> gtk4::TextView {
    widgets(app)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk4::TextView>().ok())
        .find(|t| t.is_editable())
        .expect("no editor")
}

fn text(app: &adw::Application) -> String {
    let b = editor(app).buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
}

/// Replace the editor's text as one user edit (marks the document modified).
fn edit(app: &adw::Application, f: impl Fn(&str) -> String) {
    let b = editor(app).buffer();
    let new = f(&text(app));
    b.begin_user_action();
    b.delete(&mut b.start_iter(), &mut b.end_iter());
    b.insert(&mut b.start_iter(), &new);
    b.end_user_action();
}

fn act(app: &adw::Application, name: &str, param: Option<glib::Variant>) {
    let win = app.active_window().unwrap();
    WidgetExt::activate_action(&win, name, param.as_ref()).unwrap();
}

fn open(app: &adw::Application, path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    act(app, "win.open-recent", Some(path.as_os_str().as_bytes().to_variant()));
}

async fn sleep(ms: u64) {
    glib::timeout_future(Duration::from_millis(ms)).await;
}

async fn wait(ms: u64, f: impl Fn() -> bool) -> bool {
    let mut left = ms as i64;
    while left > 0 {
        if f() {
            return true;
        }
        sleep(50).await;
        left -= 50;
    }
    f()
}

/// Drop whatever toasts are up, so the next check sees only new ones.
async fn clear_toasts(app: &adw::Application) {
    // Dismissing the shown toast reveals the next queued one: drain them.
    for _ in 0..20 {
        let shown: Vec<_> = widgets(app).into_iter().filter(|w| w.type_().name() == "AdwToastWidget").collect();
        if shown.is_empty() {
            break;
        }
        for w in shown {
            let _ = w.activate_action("toast.dismiss", None);
        }
        sleep(400).await;
    }
}

fn remove_link(t: &str) -> String {
    t.lines().filter(|l| !l.starts_with("![pic]")).collect::<Vec<_>>().join("\n") + "\n"
}

struct Report(RefCell<u32>);
impl Report {
    fn check(&self, name: &str, ok: bool) {
        eprintln!("{} {name}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            if let Some(app) = gio::Application::default().and_downcast::<adw::Application>() {
                eprintln!("     labels: {:?}", labels(&app).into_iter().filter(|l| l.len() > 3).collect::<Vec<_>>());
            }
        }
        if !ok {
            *self.0.borrow_mut() += 1;
        }
    }
}

async fn load(app: &adw::Application, path: &Path, marker: &str) -> bool {
    open(app, path);
    wait(4000, || text(app).contains(marker)).await
}

async fn save_and_toast(app: &adw::Application, toast: &str) -> bool {
    act(app, "win.save", None);
    wait(6000, || has_label(app, toast)).await
}

async fn no_toast(app: &adw::Application) -> bool {
    act(app, "win.save", None);
    // The "Saved" toast holds 2 s; a suggestion would queue behind it.
    sleep(3500).await;
    !has_label(app, "no longer used")
}

// --- scenarios ---

async fn run(app: &adw::Application) -> u32 {
    let r = Rc::new(Report(RefCell::new(0)));
    sleep(500).await;

    // 1. Plain: remove the link, save, toast → dialog → trash.
    {
        let dir = case_dir("s1");
        let (doc, img, name) = plain_doc(&dir, "A1", "");
        r.check("s1 loads", load(app, &doc, "# A1").await);
        edit(app, remove_link);
        r.check("s1 toast after save", save_and_toast(app, "1 image no longer used in A1.md").await);
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s1 dialog", wait(4000, || button(app, "Move to Trash").is_some()).await);
        r.check("s1 image pre-ticked", checks(app) == vec![true]);
        button(app, "Move to Trash").unwrap().emit_clicked();
        r.check("s1 image gone", wait(4000, || !img.exists()).await);
        r.check("s1 image in Trash", trash_dir().join(&name).exists());
        r.check("s1 undo cleared", wait(1000, || !editor(app).buffer().can_undo()).await);
        clear_toasts(app).await;
    }

    // 2. Name still written in the text (code span): no suggestion.
    {
        let dir = case_dir("s2");
        let (doc, img, name) = plain_doc(&dir, "A2", "");
        fs::write(&doc, format!("# A2\n\n![pic](assets/{name})\n\n`assets/{name}`\n")).unwrap();
        r.check("s2 loads", load(app, &doc, "# A2").await);
        edit(app, remove_link);
        r.check("s2 no toast (still mentioned)", no_toast(app).await);
        r.check("s2 image kept", img.exists());
        clear_toasts(app).await;
    }

    // 3. A sibling document uses the image: the scan disagrees, no toast.
    {
        let dir = case_dir("s3");
        let (doc, img, name) = plain_doc(&dir, "A3", "");
        fs::write(dir.join("B3.md"), format!("![x](assets/{name})\n")).unwrap();
        r.check("s3 loads", load(app, &doc, "# A3").await);
        edit(app, remove_link);
        r.check("s3 no toast (sibling uses it)", no_toast(app).await);
        r.check("s3 image kept", img.exists());
        clear_toasts(app).await;
    }

    // 4. Setting off: no suggestion.
    {
        let dir = case_dir("s4");
        let (doc, img, _) = plain_doc(&dir, "A4", "");
        r.check("s4 loads", load(app, &doc, "# A4").await);
        act(app, "win.suggest-cleanup", None);
        edit(app, remove_link);
        r.check("s4 no toast (setting off)", no_toast(app).await);
        r.check("s4 image kept", img.exists());
        act(app, "win.suggest-cleanup", None);
        clear_toasts(app).await;
    }

    // 5. Save from the Save/Discard question before opening another file.
    {
        let dir = case_dir("s5");
        let (doc, img, _) = plain_doc(&dir, "A5", "");
        let other = dir.join("B5.md");
        fs::write(&other, "# B5\n\nOther.\n").unwrap();
        r.check("s5 loads", load(app, &doc, "# A5").await);
        edit(app, remove_link);
        open(app, &other);
        r.check("s5 asks", wait(3000, || button(app, "Save").is_some()).await);
        button(app, "Save").unwrap().emit_clicked();
        r.check("s5 B5 opened", wait(4000, || text(app).contains("# B5")).await);
        r.check("s5 toast names A5", wait(6000, || has_label(app, "1 image no longer used in A5.md")).await);
        edit(app, |t| format!("{t}typed in B5\n"));
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s5 dialog", wait(4000, || button(app, "Move to Trash").is_some()).await);
        r.check("s5 dialog names A5", labels(app).iter().any(|l| l.starts_with("A5.md")));
        button(app, "Move to Trash").unwrap().emit_clicked();
        r.check("s5 A5 image gone", wait(4000, || !img.exists()).await);
        r.check("s5 B5 undo kept", editor(app).buffer().can_undo());
        r.check("s5 B5 text untouched", text(app).contains("typed in B5"));
        // Leave B5 unmodified for the next open.
        act(app, "win.save", None);
        sleep(300).await;
        clear_toasts(app).await;
    }

    // 6. Another document starts using the image while the dialog is open.
    {
        let dir = case_dir("s6");
        let (doc, img, name) = plain_doc(&dir, "A6", "");
        r.check("s6 loads", load(app, &doc, "# A6").await);
        edit(app, remove_link);
        r.check("s6 toast", save_and_toast(app, "no longer used in A6.md").await);
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s6 dialog", wait(4000, || button(app, "Move to Trash").is_some()).await);
        fs::write(dir.join("C6.md"), format!("![copy](assets/{name})\n")).unwrap();
        button(app, "Move to Trash").unwrap().emit_clicked();
        r.check("s6 kept toast", wait(4000, || has_label(app, "Kept 1 image")).await);
        sleep(500).await;
        r.check("s6 image kept", img.exists());
        clear_toasts(app).await;
    }

    // 7. A newer save of the same document dismisses the stale offer.
    {
        let dir = case_dir("s7");
        let (doc, _, _) = plain_doc(&dir, "A7", "");
        r.check("s7 loads", load(app, &doc, "# A7").await);
        edit(app, remove_link);
        r.check("s7 toast", save_and_toast(app, "no longer used in A7.md").await);
        edit(app, |t| format!("{t}more\n"));
        act(app, "win.save", None);
        r.check("s7 offer gone", wait(2000, || button(app, "Clean Up…").is_none()).await);
        clear_toasts(app).await;
    }

    // 8. Edited after the offer: clicking it refuses.
    {
        let dir = case_dir("s8");
        let (doc, img, _) = plain_doc(&dir, "A8", "");
        r.check("s8 loads", load(app, &doc, "# A8").await);
        edit(app, remove_link);
        r.check("s8 toast", save_and_toast(app, "no longer used in A8.md").await);
        edit(app, |t| format!("{t}more\n"));
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s8 refused", wait(4000, || has_label(app, "save the document first")).await);
        r.check("s8 no dialog", button(app, "Move to Trash").is_none());
        r.check("s8 image kept", img.exists());
        act(app, "win.save", None);
        sleep(300).await;
        clear_toasts(app).await;
    }

    // 9. The open document is rewritten by another program after the offer
    //    (linking the image again): clicking it must not trash.
    {
        let dir = case_dir("s9");
        let (doc, img, name) = plain_doc(&dir, "A9", "");
        r.check("s9 loads", load(app, &doc, "# A9").await);
        edit(app, remove_link);
        r.check("s9 toast", save_and_toast(app, "no longer used in A9.md").await);
        sleep(1100).await; // a coarse mtime must still move
        let mut f = fs::OpenOptions::new().append(true).open(&doc).unwrap();
        writeln!(f, "![pic](assets/{name})").unwrap();
        drop(f);
        button(app, "Clean Up…").unwrap().emit_clicked();
        sleep(1500).await;
        if let Some(b) = button(app, "Move to Trash") {
            b.emit_clicked();
        }
        sleep(1000).await;
        r.check("s9 image kept (changed on disk)", img.exists());
        if let Some(b) = button(app, "Cancel") {
            b.emit_clicked();
        }
        clear_toasts(app).await;
    }

    // 10. TextBundle: the dropped image ticked, an older orphan not.
    {
        let dir = root().join("s10").join("A10.textbundle");
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("info.json"), r#"{"version":2,"type":"net.daringfireball.markdown"}"#).unwrap();
        fs::write(dir.join("assets/pic.png"), png()).unwrap();
        fs::write(dir.join("assets/old.png"), png()).unwrap();
        fs::write(dir.join("text.md"), "# A10\n\n![pic](assets/pic.png)\n").unwrap();
        r.check("s10 loads", load(app, &dir, "# A10").await);
        edit(app, remove_link);
        r.check("s10 toast", save_and_toast(app, "1 image no longer used in A10.textbundle").await);
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s10 dialog", wait(4000, || button(app, "Move to Trash").is_some()).await);
        // Sorted as listed: old.png, pic.png.
        let ticks = checks(app);
        r.check(&format!("s10 only pic ticked {ticks:?}"), ticks.len() == 2 && ticks.iter().filter(|t| **t).count() == 1);
        button(app, "Move to Trash").unwrap().emit_clicked();
        r.check("s10 pic gone", wait(4000, || !dir.join("assets/pic.png").exists()).await);
        r.check("s10 old kept", dir.join("assets/old.png").exists());
        clear_toasts(app).await;
    }

    // 11. TextPack: the entry is taken out of the zip.
    {
        let dir = root().join("s11");
        fs::create_dir_all(&dir).unwrap();
        let pack = dir.join("A11.textpack");
        {
            let mut zip = zip::ZipWriter::new(fs::File::create(&pack).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("info.json", opts).unwrap();
            zip.write_all(br#"{"version":2,"type":"net.daringfireball.markdown"}"#).unwrap();
            zip.start_file("text.md", opts).unwrap();
            zip.write_all(b"# A11\n\n![pic](assets/pic.png)\n").unwrap();
            zip.start_file("assets/pic.png", opts).unwrap();
            zip.write_all(&png()).unwrap();
            zip.finish().unwrap();
        }
        let entries = |p: &Path| {
            let z = zip::ZipArchive::new(fs::File::open(p).unwrap()).unwrap();
            z.file_names().map(String::from).collect::<Vec<_>>()
        };
        r.check("s11 loads", load(app, &pack, "# A11").await);
        edit(app, remove_link);
        r.check("s11 toast", save_and_toast(app, "1 image no longer used in A11.textpack").await);
        button(app, "Clean Up…").unwrap().emit_clicked();
        r.check("s11 dialog", wait(4000, || button(app, "Move to Trash").is_some()).await);
        button(app, "Move to Trash").unwrap().emit_clicked();
        r.check("s11 entry gone", wait(4000, || !entries(&pack).iter().any(|e| e == "assets/pic.png")).await);
        r.check("s11 text kept", entries(&pack).iter().any(|e| e == "text.md"));
        r.check("s11 pic in Trash", trash_dir().join("pic.png").exists());
        r.check("s11 no changed-on-disk banner", !has_label(app, "changed on disk"));
    }

    let n = *r.0.borrow();
    n
}
