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

use std::collections::HashMap;
use std::io::Write;

use zvariant::Value;

fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// The tooltip's own type: an icon name, its pixmaps, and the two strings
/// anybody actually reads.
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

struct Item {
    title: String,
    /// Whether this item publishes a menu object at all. An item that does not
    /// answers `/` — a valid path pointing at nothing — and is expected to draw
    /// its own window when `ContextMenu` arrives.
    menu: bool,
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

    /// Where the menu is. An item with no menu publishes `/` here.
    #[zbus(property)]
    fn menu(&self) -> zvariant::ObjectPath<'_> {
        let path = if self.menu { MENU_PATH } else { "/" };
        zvariant::ObjectPath::from_static_str(path).expect("a valid path")
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

/// Where this item keeps its menu. An item with none publishes `/`, which is
/// what the compositor reads as "ask the application to draw its own".
const MENU_PATH: &str = "/MenuBar";

/// One node of a layout: an id, its properties, and its children.
type Node<'a> = (i32, HashMap<String, Value<'a>>, Vec<Value<'a>>);

/// The menu object, which is a separate interface on a separate path — the
/// tray specification says nothing about menus, and this is Canonical's.
struct Menu;

impl Menu {
    /// One row. Properties are an open map, and a row that carries none is the
    /// common case: everything below defaults.
    fn row(id: i32, label: &str, props: Vec<(&str, Value<'static>)>) -> Value<'static> {
        let mut map: HashMap<String, Value<'static>> = HashMap::new();
        if !label.is_empty() {
            map.insert("label".to_owned(), Value::from(label.to_owned()));
        }
        for (name, value) in props {
            map.insert(name.to_owned(), value);
        }
        Value::from((id, map, Vec::<Value<'static>>::new()))
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    /// The whole menu, in the shape the specification names `(ia{sv}av)`.
    fn get_layout(
        &self,
        _parent: i32,
        _depth: i32,
        _properties: Vec<String>,
    ) -> (u32, Node<'static>) {
        let submenu = (
            5i32,
            HashMap::from([
                ("label".to_owned(), Value::from("Recent".to_owned())),
                (
                    "children-display".to_owned(),
                    Value::from("submenu".to_owned()),
                ),
            ]),
            vec![Self::row(6, "notes.md", vec![])],
        );

        let children = vec![
            // The label carries a mnemonic, as a real menu's does.
            Self::row(1, "_Open", vec![]),
            Self::row(2, "", vec![("type", Value::from("separator".to_owned()))]),
            Self::row(3, "Sync now", vec![("enabled", Value::from(false))]),
            Self::row(
                4,
                "Automatic",
                vec![
                    ("toggle-type", Value::from("checkmark".to_owned())),
                    ("toggle-state", Value::from(1i32)),
                ],
            ),
            // A row nobody should see, which is how an application hides one
            // rather than leaving it out.
            Self::row(7, "Hidden", vec![("visible", Value::from(false))]),
            Value::from(submenu),
        ];

        (1, (0, HashMap::new(), children))
    }

    /// Whether the menu changed since it was last fetched. Answering false is
    /// honest here — this menu is fixed — and the compositor fetches anyway.
    fn about_to_show(&self, _id: i32) -> bool {
        say("menu about to show");
        false
    }

    fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
        say(&format!("menu event {id} {event_id}"));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--no-menu` publishes no menu object, which is the other half of the
    // desktop: an item that expects the host to send it `ContextMenu` and
    // draws its own window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let menu = !args.iter().any(|arg| arg == "--no-menu");
    let title = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "Test".to_owned());

    let mut builder = zbus::blocking::connection::Builder::session()?
        .serve_at("/StatusNotifierItem", Item { title, menu })?;
    if menu {
        builder = builder.serve_at(MENU_PATH, Menu)?;
    }
    let connection = builder.build()?;

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
