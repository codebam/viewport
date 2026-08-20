// SPDX-License-Identifier: GPL-3.0-or-later
//
// A program holding the screen awake, for testing the idle timer without a
// video player.
//
// What a media player does while something is playing: take a hold on
// `org.freedesktop.ScreenSaver` and keep the connection open for as long as it
// is playing. That second half is the part no `gdbus call` can stand in for —
// a command-line call is a connection that dies the instant it has its answer,
// which tests the *release* path and cannot test the hold.
//
// It prints the cookie it was given and then waits to be killed, because that
// is what `tests/inhibit.test.sh` reads and how it ends. Killing it is also a
// case worth testing: nothing calls `UnInhibit` on the way out, exactly like a
// player that crashed.
//
//   cargo run --example inhibit-holder -- [name] [reason]

use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "test-holder".to_owned());
    let reason = args.next().unwrap_or_else(|| "playing video".to_owned());

    let connection = zbus::blocking::Connection::session()?;
    let cookie: u32 = connection
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "Inhibit",
            &(name.as_str(), reason.as_str()),
        )?
        .body()
        .deserialize()?;

    println!("held {cookie}");
    let _ = std::io::stdout().flush();

    // The connection is what the hold hangs on, so this has to stay alive
    // rather than return. Nothing here releases it: the test kills this
    // process to check that the compositor notices the connection going.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
