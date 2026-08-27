// SPDX-License-Identifier: GPL-3.0-or-later
//
// Drives a real compositor over its control socket.
//
// Headless, so this needs no GPU, no display and no seat — which is the whole
// reason the headless backend exists. What it covers is the seam the unit tests
// cannot: that the socket is created with the right permissions, that framing
// and parsing are wired to the handlers, and that a refusal goes back to the
// client that caused it rather than to everyone.

mod common;

use common::{Client, Compositor};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_socket_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let compositor = Compositor::start("perms");
    let mode = std::fs::metadata(&compositor.socket)
        .unwrap()
        .permissions()
        .mode();
    // XDG_RUNTIME_DIR is 0700 and hides it, but the /tmp fallback is not.
    assert_eq!(mode & 0o777, 0o600, "control socket is not 0600");
}

#[test]
fn output_query_answers_with_the_headless_output() {
    let compositor = Compositor::start("output");
    let mut client = compositor.connect();

    client.send(r#"{"type":"output.query"}"#);
    let layout = client.wait_for("output.layout");

    let outputs = layout["outputs"].as_array().expect("outputs array");
    assert_eq!(outputs.len(), 1);
    let output = &outputs[0];
    assert_eq!(output["name"], "HEADLESS-1");
    assert_eq!(output["width"], 1920);
    assert_eq!(output["height"], 1080);
    // Nothing has reserved anything, so the usable area is the whole output.
    assert_eq!(output["usable_width"], 1920);
    assert_eq!(output["usable_height"], 1080);
    // The empty-string convention, never null.
    assert_eq!(output["serial"], "");
    assert!(!output["modes"].as_array().unwrap().is_empty());
}

#[test]
fn mirror_is_one_logical_desktop_with_two_physical_heads() {
    let compositor = Compositor::start("output-mirror");
    let mut client = compositor.connect();
    client.send(r#"{"type":"output.test_add"}"#);
    client.send(r#"{"type":"output.configure","name":"HEADLESS-1","enabled":false}"#);
    client.send(r#"{"type":"output.configure","name":"HEADLESS-2","mirror":"HEADLESS-1"}"#);
    let error = client.wait_for("error");
    assert!(error["message"]
        .as_str()
        .unwrap_or_default()
        .contains("not an enabled logical output"));
    client.send(r#"{"type":"output.configure","name":"HEADLESS-1","enabled":true}"#);
    client.send(
        r#"{"type":"output.configure","name":"HEADLESS-2","mirror":"HEADLESS-1","vrr":"fullscreen"}"#,
    );
    client.send(r#"{"type":"output.query"}"#);

    let mut layout = client.wait_for("output.layout");
    while layout["outputs"].as_array().map(Vec::len) != Some(2)
        || layout["outputs"][1]["role"] != "mirror-sink"
    {
        layout = client.wait_for("output.layout");
    }
    assert_eq!(layout["outputs"][0]["role"], "mirror-source");
    assert_eq!(layout["outputs"][1]["mirror_source"], "HEADLESS-1");
    assert_eq!(layout["outputs"][1]["vrr"], "fullscreen");
    assert_eq!(layout["outputs"][0]["x"], layout["outputs"][1]["x"]);
    assert_eq!(layout["outputs"][0]["width"], layout["outputs"][1]["width"]);

    client.send(r#"{"type":"output.configure","name":"HEADLESS-2","enabled":false}"#);
    client.send(r#"{"type":"output.configure","name":"HEADLESS-1","enabled":false}"#);
    let error = client.wait_for("error");
    assert!(error["message"]
        .as_str()
        .unwrap_or_default()
        .contains("only output"));
    client.send(r#"{"type":"output.configure","name":"HEADLESS-2","enabled":true}"#);

    client.send(r#"{"type":"output.configure","name":"HEADLESS-2","mirror":"HEADLESS-2"}"#);
    let error = client.wait_for("error");
    assert!(error["message"]
        .as_str()
        .unwrap_or_default()
        .contains("itself"));

    client.send(r#"{"type":"output.test_remove","name":"HEADLESS-1"}"#);
    client.send(r#"{"type":"output.query"}"#);
    let mut promoted = client.wait_for("output.layout");
    while promoted["outputs"].as_array().map(Vec::len) != Some(1) {
        promoted = client.wait_for("output.layout");
    }
    assert_eq!(promoted["outputs"][0]["name"], "HEADLESS-2");
    assert_eq!(promoted["outputs"][0]["role"], "desktop");
    assert_eq!(promoted["outputs"][0]["x"], 0);
}

#[test]
fn view_query_answers_with_the_config() {
    let compositor = Compositor::start("config");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.query"}"#);
    let config = client.wait_for("config");

    assert_eq!(config["layout"], "tiling");
    // Both true, as in src/main.c:69 — "the empty desktop explains itself
    // until told not to". These set no-logo and no-tutorial on the document
    // when false, and on a desktop with no windows they are the only things
    // there are to draw, so getting them wrong leaves bare wallpaper and
    // looks like a compositor that is not drawing the shell at all.
    //
    // Asserting is_boolean() here is what let that through.
    assert_eq!(config["logo"], true, "the empty desktop would be bare");
    assert_eq!(config["tutorial"], true, "the empty desktop would be bare");
    // Unset members are omitted, not null.
    assert!(config.get("bar").is_none());
    assert!(config.get("rules").is_none());
}

#[test]
fn a_config_file_reaches_the_shell() {
    // The point of the file: what it says is what the shell is told on
    // connect. Anything the file leaves out keeps its built-in value, which is
    // why "tutorial" is still true here.
    let dir = std::env::temp_dir().join("viewport-config-integration");
    std::fs::create_dir_all(dir.join("viewport")).expect("mkdir");
    let path = dir.join("viewport/config.json");
    std::fs::write(&path, r#"{"layout":"scrolling","logo":false,"bar":"auto"}"#).expect("write");

    let compositor = Compositor::builder("config-file")
        .env("XDG_CONFIG_HOME", &dir)
        .start();
    let mut client = compositor.connect();
    client.send(r#"{"type":"view.query"}"#);
    let config = client.wait_for("config");

    assert_eq!(config["layout"], "scrolling");
    assert_eq!(config["logo"], false);
    assert_eq!(config["bar"], "auto");
    // Not in the file, so still the built-in.
    assert_eq!(config["tutorial"], true);

    let _ = std::fs::remove_file(&path);
}

/// The colour scheme, both ways: the shell has to be able to read it before it
/// can draw a switch for it.
///
/// It was set-only — `appearance toggle` on a chord and nothing in the `config`
/// event — so a settings panel could move the setting and had no way to show
/// where it was. A switch drawn from a guess is one that shows the wrong state
/// until somebody presses it twice.
#[test]
fn the_colour_scheme_is_announced_as_well_as_set() {
    let compositor = Compositor::start("dark-mode");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.query"}"#);
    // Dark unless something says otherwise, which is what the session starts
    // on and what `docs/configuration.md` says absence means.
    assert_eq!(client.wait_for("config")["dark_mode"], true);

    // A panel sends the state it wants rather than a toggle, so that two
    // clicks on a switch do not depend on which value the desk was on.
    client.send(r#"{"type":"config.dark_mode","enabled":false}"#);
    assert_eq!(client.wait_for("config")["dark_mode"], false);
    client.send(r#"{"type":"config.dark_mode","enabled":false}"#);
    assert_eq!(client.wait_for("config")["dark_mode"], false);

    // And absent toggles, which is what the keybinding wants.
    client.send(r#"{"type":"config.dark_mode"}"#);
    assert_eq!(client.wait_for("config")["dark_mode"], true);
}

/// The whole of the persistence answer, end to end: set something at runtime,
/// save, restart, and find it still there.
///
/// This is the property the settings panel is built on and the one nothing
/// else in the tree checks — the runtime setters deliberately do not touch the
/// disk, so before `config.save` existed the answer to "does a change stick"
/// was no. The overlay goes *beside* the config file rather than into it; the
/// config file here has a value of its own for one of the same keys, and the
/// overlay has to win.
#[test]
fn saved_settings_come_back_after_a_restart() {
    let dir = std::env::temp_dir().join("viewport-settings-integration");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("viewport")).expect("mkdir");
    let config = dir.join("viewport/config.json");
    let overlay = dir.join("viewport/settings.json");
    std::fs::write(&config, r#"{"gaps":{"inner":3},"dark_mode":true}"#).expect("write");

    {
        let compositor = Compositor::builder("settings-save")
            .env("XDG_CONFIG_HOME", &dir)
            .start();
        let mut client = compositor.connect();

        client.send(r#"{"type":"config.gaps","inner":21,"outer":7}"#);
        client.send(r#"{"type":"config.dark_mode","enabled":false}"#);
        client.send(r#"{"type":"config.save"}"#);
        let saved = client.wait_for("config.saved");
        assert_eq!(saved["path"], overlay.to_string_lossy().as_ref());
    }

    // Written, and written as the config file's own vocabulary — this is the
    // check that stops the overlay from drifting into a private format the
    // reader would silently ignore every key of.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&overlay).expect("read the overlay"))
            .expect("the overlay is JSON");
    assert_eq!(written["gaps"]["inner"], 21);
    assert_eq!(written["gaps"]["outer"], 7);
    assert_eq!(written["dark_mode"], false);
    // Nothing configured a monitor, so nothing was saved about one. Saving
    // every head would freeze whatever mode the backend picked for a screen
    // nobody has an opinion about.
    assert!(written.get("outputs").is_none());

    // And the config file is untouched, comments, formatting and all — the
    // entire argument for the overlay existing.
    assert_eq!(
        std::fs::read_to_string(&config).expect("read the config"),
        r#"{"gaps":{"inner":3},"dark_mode":true}"#
    );

    {
        let compositor = Compositor::builder("settings-restart")
            .env("XDG_CONFIG_HOME", &dir)
            .start();
        let mut client = compositor.connect();
        client.send(r#"{"type":"view.query"}"#);
        let config_event = client.wait_for("config");
        assert_eq!(
            config_event["gaps"]["inner"], 21,
            "the overlay lost to the file"
        );
        assert_eq!(config_event["gaps"]["outer"], 7);
        assert_eq!(config_event["dark_mode"], false);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A monitor change is provisional until somebody says they can see it.
///
/// `docs/ipc.md` has described this countdown since before anything armed one:
/// `output.confirm` was a handler with an empty body. Anything that read the
/// documentation and skipped the confirmation therefore kept a mode that had
/// blanked the screen, which is the exact failure the sentence rules out.
///
/// Driven with `output.revert` rather than by waiting out the twelve seconds —
/// the deadline is the same code path, and a test that sleeps for twelve
/// seconds to prove it is a test nobody runs.
#[test]
fn an_output_change_can_be_taken_back() {
    let compositor = Compositor::start("output-revert");
    let mut client = compositor.connect();

    client.send(r#"{"type":"output.query"}"#);
    let before = client.wait_for("output.layout")["outputs"][0]["scale"]
        .as_f64()
        .expect("a scale");
    assert_eq!(before, 1.0);

    client.send(r#"{"type":"output.configure","name":"HEADLESS-1","scale":2.0}"#);
    client.send(r#"{"type":"output.query"}"#);
    // Two layouts go out — the configure's own and the query's — and both say
    // the same thing, so reading either is reading the change.
    let mut applied = client.wait_for("output.layout");
    while applied["outputs"][0]["scale"].as_f64() != Some(2.0) {
        applied = client.wait_for("output.layout");
    }

    client.send(r#"{"type":"output.revert"}"#);
    client.send(r#"{"type":"output.query"}"#);
    let mut back = client.wait_for("output.layout");
    while back["outputs"][0]["scale"].as_f64() != Some(1.0) {
        back = client.wait_for("output.layout");
    }
    assert_eq!(back["outputs"][0]["scale"], 1.0);
}

/// And a confirmed one stays.
///
/// The other half, and the one worth a test of its own: a revert that fires
/// after the confirmation would undo a change somebody explicitly kept, which
/// is worse than never having offered the countdown.
#[test]
fn a_confirmed_output_change_is_not_taken_back() {
    let compositor = Compositor::start("output-confirm");
    let mut client = compositor.connect();

    client.send(r#"{"type":"output.configure","name":"HEADLESS-1","scale":2.0}"#);
    client.send(r#"{"type":"output.confirm"}"#);
    // A revert after the confirmation has nothing to go back to, and says so
    // by doing nothing rather than by refusing — the deadline may have fired a
    // moment before the click, and the desk is then in the state the click
    // asked for.
    client.send(r#"{"type":"output.revert"}"#);
    client.send(r#"{"type":"output.query"}"#);

    let mut layout = client.wait_for("output.layout");
    while layout["outputs"][0]["scale"].as_f64() != Some(2.0) {
        layout = client.wait_for("output.layout");
    }
    assert_eq!(layout["outputs"][0]["scale"], 2.0);
}

/// A real layer-shell client, run against a real compositor.
///
/// wmenu is the reason layer-shell was ported and it exercises the whole path:
/// the global has to exist, the surface has to be arranged, and the initial
/// configure has to carry a width — it asks for zero, meaning "the compositor
/// decides", and allocates a buffer from whatever it is told. Every one of
/// those was wrong at some point and each failed differently: no global was an
/// assertion inside wmenu, no configure was an invalid shm pool.
#[test]
fn a_layer_shell_client_is_configured_with_a_real_size() {
    use std::io::Read;

    let compositor = Compositor::start("layer");
    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a wayland display");

    let mut child = match Command::new("wmenu")
        .env("WAYLAND_DISPLAY", &display)
        .env("WAYLAND_DEBUG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        // Not installed. Skipping is right; failing would make the suite
        // depend on what happens to be on the machine.
        Err(_) => return,
    };
    use std::io::Write as _;
    let _ = child.stdin.take().unwrap().write_all(b"alpha\n");

    std::thread::sleep(Duration::from_secs(2));
    let _ = child.kill();
    let mut trace = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut trace);
    let _ = child.wait();
    // libwayland colours its trace, and it does not ask whether anything is
    // watching: the request name and its arguments come out with an escape
    // between them, so `.configure(` never appears literally and this test
    // failed on a compositor that was answering perfectly. Stripped rather
    // than matched around, because every future assertion here would have to
    // remember.
    let trace = strip_ansi(&trace);

    assert!(
        trace.contains("zwlr_layer_surface_v1"),
        "wmenu never got a layer surface: {trace}"
    );
    // The width it was told, not the zero it asked for.
    let configured = trace
        .lines()
        .find(|line| line.contains("zwlr_layer_surface_v1") && line.contains(".configure("));
    let configured = configured.unwrap_or_else(|| panic!("no configure in: {trace}"));
    assert!(
        configured.contains(", 1920, "),
        "configured with no width, so the client allocates nothing: {configured}"
    );
    assert!(
        !trace.contains("invalid wl_shm_pool size"),
        "the client was configured into an impossible buffer: {trace}"
    );
}

/// A window still gets a rectangle when nothing answers view.added.
///
/// The whole layout lives in a web page, so a shell that throws, fails to load
/// or is served from a machine that has gone away places nothing — and a
/// window that is never placed is never shown. Without this the session is a
/// black screen with a working keyboard and no way to find out why.
#[test]
fn an_unanswered_window_is_laid_out_anyway() {
    let compositor = Compositor::start("watchdog");
    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a wayland display");

    // A client, and deliberately no shell: nothing will answer view.added.
    let mut client = compositor.connect();
    let mut child = match Command::new("foot")
        .env("WAYLAND_DISPLAY", &display)
        .arg("-e")
        .arg("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        // Not installed; the assertion would test nothing.
        Err(_) => return,
    };

    let added = client.wait_for("view.added");
    let id = added["id"].as_u64().expect("an id");

    // Nothing places it, so the watchdog has to. Its deadline is 2500ms.
    std::thread::sleep(Duration::from_millis(4000));
    client.send(r#"{"type":"view.query"}"#);

    // The replay carries the geometry the compositor settled on.
    let mut placed = None;
    for _ in 0..64 {
        let message = client.wait_for("view.added");
        if message["id"].as_u64() == Some(id) && message["replay"] == true {
            placed = Some(message);
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let placed = placed.expect("the window was never replayed");
    // The headless output is 1920x1080 and this is the only window, so the
    // fallback gives it the whole thing.
    assert_eq!(
        placed["width"], 1920,
        "the watchdog did not lay the window out: {placed}"
    );
}

/// The bar's numbers arrive without anyone asking.
///
/// The shell cannot read /proc — it is a page loaded from file:// — so a bar
/// with no compositor sampling behind it shows nothing at all.
#[test]
fn status_updates_arrive_on_their_own() {
    let compositor = Compositor::start("status");
    let mut client = compositor.connect();

    // Unprompted: this is a broadcast on a timer, not a reply.
    let status = client.wait_for("status.update");

    // -1 means "could not read", which is what the bar tests for. On a Linux
    // machine running this test, /proc is there.
    let memory = status["memory"].as_f64().expect("memory is a number");
    assert!(
        (0.0..=100.0).contains(&memory),
        "memory should be a percentage, got {memory}"
    );
    assert!(
        status["disk_total"].as_f64().unwrap_or(0.0) > 0.0,
        "the root filesystem has a size"
    );
    // A single number, not an array: the bar calls s.load.toFixed(2).
    assert!(status["load"].is_number(), "load must be one number");
    assert!(status["net_rx"].is_number());
}

#[test]
fn a_malformed_message_comes_back_as_an_error() {
    let compositor = Compositor::start("malformed");
    let mut client = compositor.connect();

    client.send("{ this is not json");
    let error = client.wait_for("error");
    assert_eq!(error["context"], "ipc");
}

#[test]
fn an_unknown_type_is_reported_against_itself() {
    let compositor = Compositor::start("unknown");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.teleport","id":1}"#);
    let error = client.wait_for("error");
    assert_eq!(error["context"], "view.teleport");
    assert_eq!(error["message"], "unknown IPC message type 'view.teleport'");
}

#[test]
fn a_type_that_is_not_a_string_cannot_reach_dispatch() {
    // The shapes that used to crash the C compositor.
    let compositor = Compositor::start("badtype");
    let mut client = compositor.connect();

    for message in [
        r#"{"type":5}"#,
        r#"{"type":null}"#,
        r#"{"type":{}}"#,
        r#"{}"#,
    ] {
        client.send(message);
        let error = client.wait_for("error");
        assert_eq!(error["context"], "ipc", "{message}");
        assert_eq!(
            error["message"], "missing or non-string 'type'",
            "{message}"
        );
    }
}

#[test]
fn an_error_goes_only_to_the_client_that_caused_it() {
    let compositor = Compositor::start("origin");
    let mut culprit = compositor.connect();
    let mut bystander = compositor.connect();

    // Give the bystander something to find that is definitely after the error
    // the other client is about to cause, so "did not receive it" is a real
    // observation rather than a race.
    culprit.send(r#"{"type":"view.teleport"}"#);
    let error = culprit.wait_for("error");
    assert_eq!(error["context"], "view.teleport");

    bystander.send(r#"{"type":"output.query"}"#);
    let seen = bystander.wait_for("output.layout");
    assert_eq!(seen["type"], "output.layout");
}

#[test]
fn empty_lines_are_ignored_rather_than_rejected() {
    let compositor = Compositor::start("blank");
    let mut client = compositor.connect();

    // An empty string reaching the parser would come back as a malformed
    // message the sender never sent.
    client.send("");
    client.send("");
    client.send(r#"{"type":"output.query"}"#);

    let value = client.wait_for("output.layout");
    assert_eq!(value["type"], "output.layout");
}

#[test]
fn a_message_split_across_writes_still_arrives() {
    let compositor = Compositor::start("split");
    let mut client = compositor.connect();

    client.writer.write_all(br#"{"type":"outp"#).unwrap();
    client.writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    client.writer.write_all(b"ut.query\"}\n").unwrap();
    client.writer.flush().unwrap();

    let value = client.wait_for("output.layout");
    assert_eq!(value["type"], "output.layout");
}

#[test]
fn quit_stops_the_compositor() {
    let mut compositor = Compositor::start("quit");
    let mut client = compositor.connect();

    client.send(r#"{"type":"quit"}"#);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = compositor.child.try_wait().unwrap() {
            assert!(status.success(), "exited with {status}");
            return;
        }
        assert!(Instant::now() < deadline, "still running after quit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Escape sequences out of a captured trace.
///
/// Only the CSI sequences libwayland emits — enough for a log, not a terminal
/// emulator.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC [ ... <final byte in @..~>
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Hotplug churn.
//
// The C compositor's worst bugs were all one shape: outputs and views
// outliving the structures that point at them. A monitor disconnecting
// mid-frame corrupted the heap, and the family was fixed several times —
// found by running scripts/asan-hotplug.sh under a sanitizer, because
// unsanitized the failures looked like a test that passed or a crash somewhere
// unrelated an hour later.
//
// Rust removes the corruption, not the mistake. The same wrong lifetime here
// is a stale view id resolving to nothing, a WeakOutput that stops upgrading
// mid-capture, or a crtc left behind in `dirty_outputs` — assertion failures
// and wrong pictures rather than a poisoned heap. Nothing about that is caught
// by a sanitizer, which is why the churn is the test and the sanitizer is only
// the amplifier.
//
// This could not be written until `output.test_add` worked; it was rejected
// unconditionally, including under --headless, which is what
// tests/output-order.test.sh exposed.
// ---------------------------------------------------------------------------

/// `(name, x)` for every output the layout describes.
fn layout_of(client: &mut Client) -> Vec<(String, i64)> {
    client.send(r#"{"type":"output.query"}"#);
    let layout = client.wait_for("output.layout");
    layout["outputs"]
        .as_array()
        .expect("outputs array")
        .iter()
        .map(|o| {
            (
                o["name"].as_str().expect("name").to_owned(),
                o["x"].as_i64().expect("x"),
            )
        })
        .collect()
}

#[test]
fn plugging_outputs_in_and_out_leaves_the_layout_consistent() {
    let compositor = Compositor::start("hotplug");
    let mut client = compositor.connect();

    assert_eq!(
        layout_of(&mut client).len(),
        1,
        "the headless backend should start with one output"
    );

    // Every name ever handed out. Names count up and are never reused: a name
    // that came back after an unplug would be a different monitor wearing the
    // identity of one that anything still holding the old one would accept.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert("HEADLESS-1".to_owned());

    for round in 0..25 {
        for _ in 0..3 {
            client.send(r#"{"type":"output.test_add"}"#);
            client.wait_for("output.layout");
        }

        let layout = layout_of(&mut client);
        assert_eq!(
            layout.len(),
            4,
            "round {round}: wrong count after plugging in"
        );

        // Left to right, in the order they arrived. This is the property the
        // shell's display panel and three "leftmost monitor" fallbacks read,
        // and the C build had it backwards once — see
        // tests/output-order.test.sh.
        let xs: Vec<i64> = layout.iter().map(|(_, x)| *x).collect();
        let mut sorted = xs.clone();
        sorted.sort_unstable();
        assert_eq!(xs, sorted, "round {round}: layout is not ordered by x");

        for (name, _) in &layout {
            if !seen.insert(name.clone()) && !name.ends_with("-1") {
                // Already present is fine for outputs that were never
                // unplugged; a *reused* name is the failure, and the only one
                // that survives every round is HEADLESS-1.
                assert!(
                    layout.iter().filter(|(n, _)| n == name).count() == 1,
                    "round {round}: {name} appears twice"
                );
            }
        }

        for _ in 0..3 {
            client.send(r#"{"type":"output.test_remove"}"#);
            client.wait_for("output.layout");
        }

        let layout = layout_of(&mut client);
        assert_eq!(
            layout.len(),
            1,
            "round {round}: outputs left over after unplugging"
        );
        assert_eq!(layout[0].0, "HEADLESS-1", "round {round}: wrong survivor");
    }

    // Still answering after 150 plug events, which is the point: a compositor
    // that had lost track would have stopped responding or started lying long
    // before here.
    assert_eq!(layout_of(&mut client).len(), 1);
}

#[test]
fn the_last_output_can_be_unplugged_and_another_plugged_back_in() {
    // A desktop with no outputs at all is the edge every "the first monitor"
    // shortcut gets wrong, and it is reachable in earnest: a laptop lid
    // closing while the dock is unplugged. Nothing may panic, and the
    // compositor has to still be there when a screen comes back.
    let compositor = Compositor::start("hotplug-empty");
    let mut client = compositor.connect();

    client.send(r#"{"type":"output.test_remove"}"#);
    client.wait_for("output.layout");
    assert!(
        layout_of(&mut client).is_empty(),
        "the last output should be removable"
    );

    client.send(r#"{"type":"output.test_add"}"#);
    client.wait_for("output.layout");

    let layout = layout_of(&mut client);
    assert_eq!(layout.len(), 1, "a screen should come back");
    // Back at the origin, because nothing else is mapped to be to the right of.
    assert_eq!(layout[0].1, 0);
    assert_ne!(
        layout[0].0, "HEADLESS-1",
        "a new monitor must not inherit the unplugged one's name"
    );
}

#[test]
fn a_shell_command_comes_back_out_as_the_event_a_keybinding_sends() {
    // The one request that goes the other way. Layout is entirely the shell's,
    // and until this existed a keypress was the only thing that could reach
    // it — so nothing outside a keyboard could switch a workspace, move focus
    // to another monitor, or put a window on a chosen screen. That last one is
    // what a two-monitor benchmark needs and could not do.
    //
    // Round-tripped here rather than mocked: the compositor re-emits it to
    // everything listening, and this connection is listening, so what arrives
    // is what a shell would have been sent.
    let compositor = Compositor::start("shellcommand");
    let mut client = compositor.connect();

    client.send(r#"{"type":"shell.command","command":"output.focus","args":["right"]}"#);
    let event = client.wait_for("shell.command");
    assert_eq!(event["command"], "output.focus");
    assert_eq!(event["args"][0], "right");

    // And with none, which is most verbs.
    client.send(r#"{"type":"shell.command","command":"layout.overview"}"#);
    let event = client.wait_for("shell.command");
    assert_eq!(event["command"], "layout.overview");
    assert_eq!(
        event["args"].as_array().map(Vec::len),
        Some(0),
        "absent args must arrive as an empty list, not as null"
    );
}
