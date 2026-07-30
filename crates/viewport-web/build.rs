// SPDX-License-Identifier: MIT
//
// Builds the WPEPlatform shim.
//
// Only when the `wpe` feature is on: everything else in this crate compiles
// and tests without WebKit anywhere near it, and keeping that true is what
// lets the protocol and buffer work be exercised on a machine that has no
// browser engine installed.

fn main() {
    println!("cargo:rerun-if-changed=shim/viewport-shim.c");
    println!("cargo:rerun-if-changed=shim/viewport-shim.h");

    if std::env::var_os("CARGO_FEATURE_WPE").is_none() {
        return;
    }

    // wpe-platform is the subclassing API; the 2.0 suffix is WPE_API_VERSION
    // 2.0, which is what ENABLE_WPE_PLATFORM selects. glib comes in through
    // it, but is named explicitly so a missing one is reported as itself.
    // wpe-webkit-2.0 brings WebKit and JavaScriptCore, which the Rust side
    // calls into directly; the others are named so a missing one is reported
    // as itself rather than as a WebKit failure.
    let deps = [
        "wpe-platform-2.0",
        "wpe-webkit-2.0",
        "glib-2.0",
        "gobject-2.0",
    ];
    let mut build = cc::Build::new();
    build.file("shim/viewport-shim.c").include("shim");

    // Probed with the metadata suppressed, so this pass only collects include
    // paths. Emitting the link flags here would put them *before* the static
    // shim on the link line, and a static archive only pulls in what has been
    // referenced by something already seen — so every WPE symbol the shim uses
    // would come back undefined.
    let mut libraries = Vec::new();
    for dep in deps {
        let library = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(dep)
            .unwrap_or_else(|e| panic!("{dep} not found: {e}"));
        for path in &library.include_paths {
            build.include(path);
        }
        libraries.push(library);
    }

    // -Wall on our own file only. WPE's headers generate plenty of warnings
    // that are not ours to fix, so they are not turned into errors here.
    build
        .flag_if_supported("-Wall")
        .flag_if_supported("-Wextra");
    // The vfunc signatures are WPE's, and several take arguments this shim has
    // no use for.
    build.flag_if_supported("-Wno-unused-parameter");

    build.compile("viewport-shim");

    // Now, so they land after -lviewport-shim.
    for library in &libraries {
        for path in &library.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        for lib in &library.libs {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }
}
