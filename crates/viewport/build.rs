// SPDX-License-Identifier: GPL-3.0-or-later
//
// Links GLib, for the main loop inversion in glib_loop.rs.
//
// viewport-web's build script emits these for its own link, and Cargo carries
// the search paths through to dependents but not reliably the library names —
// so a crate that calls GLib itself has to ask for it itself. Without this the
// failure is "DSO missing from command line", which names the symptom and not
// the cause.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_WPE").is_none() {
        return;
    }

    for dep in ["glib-2.0", "gobject-2.0"] {
        pkg_config::Config::new()
            .probe(dep)
            .unwrap_or_else(|e| panic!("{dep} not found: {e}"));
    }
}
