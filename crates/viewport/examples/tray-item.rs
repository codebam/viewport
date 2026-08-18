// SPDX-License-Identifier: GPL-3.0-or-later
//
// A tray item, for testing the compositor's tray without a desktop.
//
// What an application does when it wants an icon: publish an object on the
// session bus, register it with whoever holds
// `org.kde.StatusNotifierWatcher`, and answer questions about what it looks
// like. This one publishes a pixmap rather than an icon name, so the test it
// serves does not depend on which icon themes are installed on the machine
// running it.
//
// It prints one line per event on stdout — the registration, and each call the
// compositor makes — because that is what `tests/tray.test.sh` reads.
//
//   cargo run --example tray-item -- [title]

use std::io::Write;

fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// The tooltip's own type: an icon name, its pixmaps, and the two strings
/// anybody actually reads.
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

struct Item {
    title: String,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".to_owned()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "viewport-tray-item".to_owned()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.title.clone()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".to_owned()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// A pixmap rather than a name: two pixels of solid red, ARGB in network
    /// byte order, which is what the specification says an item sends.
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let pixel = [0xff, 0xff, 0x00, 0x00];
        vec![(2, 2, pixel.repeat(4))]
    }

    #[zbus(property)]
    fn tool_tip(&self) -> ToolTip {
        (
            String::new(),
            Vec::new(),
            "Test item".to_owned(),
            "with a body".to_owned(),
        )
    }

    fn activate(&self, x: i32, y: i32) {
        say(&format!("activate {x} {y}"));
    }

    fn secondary_activate(&self, x: i32, y: i32) {
        say(&format!("secondary {x} {y}"));
    }

    fn context_menu(&self, x: i32, y: i32) {
        say(&format!("menu {x} {y}"));
    }

    fn scroll(&self, delta: i32, orientation: String) {
        say(&format!("scroll {delta} {orientation}"));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let title = std::env::args().nth(1).unwrap_or_else(|| "Test".to_owned());

    let connection = zbus::blocking::connection::Builder::session()?
        .serve_at("/StatusNotifierItem", Item { title })?
        .build()?;

    // The name an item takes, as every toolkit spells it. Nothing reads it
    // here — the compositor is told the unique name below — but an
    // implementation that watched for well-known names would need it.
    let well_known = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    connection.request_name(well_known.as_str())?;

    let unique = connection
        .unique_name()
        .map(ToString::to_string)
        .unwrap_or_default();
    connection.call_method(
        Some("org.kde.StatusNotifierWatcher"),
        "/StatusNotifierWatcher",
        Some("org.kde.StatusNotifierWatcher"),
        "RegisterStatusNotifierItem",
        &(unique.as_str()),
    )?;
    say(&format!("registered {unique}"));

    // Until it is killed. The connection answers on its own threads.
    loop {
        std::thread::park();
    }
}
