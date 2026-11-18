//! Compiles `data/bindle.gresource.xml` (the app's own symbolic icons) into
//! a GResource bundle that `ui::load_css` registers from memory. Calls
//! `glib-compile-resources` directly — it ships with glib2-devel, which
//! GTK's own -devel package already pulls in, and with the GNOME Flatpak SDK.
use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindle.gresource");
    println!("cargo:rerun-if-changed=data/bindle.gresource.xml");
    println!("cargo:rerun-if-changed=data/icons");
    let status = Command::new("glib-compile-resources")
        .args(["--sourcedir", "data", "--target"])
        .arg(&out)
        .arg("data/bindle.gresource.xml")
        .status()
        .expect("glib-compile-resources not found (install glib2-devel)");
    assert!(status.success(), "glib-compile-resources failed");
}
