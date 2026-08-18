// SPDX-License-Identifier: GPL-3.0-or-later
//
// The system tray.
//
// Nothing on a Linux desktop puts an icon in a tray. It registers itself with
// whichever program has claimed `org.kde.StatusNotifierWatcher` and waits to be
// asked what it looks like — the protocol KDE wrote, GNOME adopted through an
// extension, and every toolkit implements. There is no Wayland protocol for
// this and there is not going to be one.
//
// So the compositor claims the name and forwards the tray to the shell, for the
// same reason it claims `org.freedesktop.Notifications`: the shell is the
// desktop, and a tray drawn by a separate bar would be a second program with a
// second configuration language, floating over a compositor that already knows
// where everything is.
//
// Three names are involved and it is worth being clear about which does what.
// The *watcher* is the registry, and there is one per session — that is the
// name this claims. A *host* is something that draws the tray, and it registers
// itself with the watcher so that items know somebody is listening; a session
// with no host makes some applications fall back to a window of their own. This
// is both, so it claims a host name as well and registers it with itself.
//
// It runs on threads of its own, like notifications and for the same reason:
// zbus wants an async runtime, this loop is GLib with calloop inside it, and
// three schedulers agreeing is worse than one channel. Every D-Bus call an item
// answers — fetching thirteen properties, activating it — happens on the worker
// thread, because an application that has stopped answering the bus must not
// take the desktop's frame loop with it.

use std::collections::HashMap;
use std::sync::mpsc;

use viewport_ipc::event::TrayItem;

/// What the tray thread sends the compositor: the whole tray, whenever any
/// part of it changes. See `Event::TrayUpdate` for why it is a snapshot.
#[derive(Debug)]
pub enum Message {
    Items(Vec<TrayItem>),
    /// One item's menu, fetched and ready for the shell to draw. Carries the
    /// position the click came with, so the menu opens under the icon.
    Menu {
        id: String,
        x: i32,
        y: i32,
        items: Vec<viewport_ipc::event::TrayMenuItem>,
    },
}

/// The interface every tray item implements, whichever toolkit wrote it.
///
/// Ayatana's fork of this specification kept the KDE interface name, so there
/// is only one to speak.
const ITEM: &str = "org.kde.StatusNotifierItem";
/// The menu interface an item points at with its `Menu` property.
///
/// Not part of the tray specification at all — it is Canonical's, written for
/// Unity, and it is what GTK and Qt both publish a menu through. An item that
/// implements neither this nor `ContextMenu` has no menu, which is allowed.
const MENU: &str = "com.canonical.dbusmenu";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Where an item lives when it registered a bus name rather than a path.
const DEFAULT_PATH: &str = "/StatusNotifierItem";

/// The size the icon is chosen for. The bar is a web page and can scale
/// whatever it is given, so this only decides which of the sizes an
/// application offers is the one worth sending.
const ICON_SIZE: u32 = 22;

/// The half of the tray the compositor keeps.
#[derive(Default)]
pub struct Tray {
    /// The worker, once it has been started. Absent means the tray has never
    /// been enabled in this session — not that it is off, which is
    /// `enabled` below.
    worker: Option<mpsc::Sender<Command>>,
    /// What the configuration last asked for. Kept so that a reload that does
    /// not mention the tray does not toggle it, and so a disabled tray does
    /// not start a bus connection it will not use.
    enabled: bool,
    /// Where a tray snapshot is sent. Absent until the event loop has a source
    /// to receive one — and absent for good in a run with no loop at all,
    /// which is what makes every method here safe to call from a test.
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl Tray {
    /// Where tray snapshots go. Called once, when the event loop has a source
    /// for them; nothing starts before this, because a tray with nowhere to
    /// send itself is a bus connection kept open for no reader.
    pub fn attach(
        &mut self,
        events: smithay::reexports::calloop::channel::Sender<Message>,
        enabled: bool,
    ) {
        self.events = Some(events);
        self.set_enabled(enabled);
    }

    /// Turn the tray on or off, as the configuration says.
    ///
    /// Called on every configuration load, including reloads, so this is both
    /// how the tray starts and how it is taken away again. Turning it off
    /// releases the bus names rather than tearing the connection down:
    /// applications watch for the watcher's name appearing and disappearing,
    /// which is exactly the signal they should get, and the thread that would
    /// have to be killed to close the connection is blocked reading from the
    /// bus.
    ///
    /// A failure to start is not fatal. A session with no D-Bus, or one where
    /// a KDE tray or a GNOME extension already holds the watcher name, still
    /// has a working compositor and a tray drawn by whoever got there first.
    pub fn set_enabled(&mut self, enabled: bool) {
        let Some(events) = self.events.clone() else {
            // Not attached yet: remember what was asked for, and let `attach`
            // act on it.
            self.enabled = enabled;
            return;
        };
        if enabled == self.enabled && (self.worker.is_some() || !enabled) {
            return;
        }
        self.enabled = enabled;

        if self.worker.is_none() {
            if !enabled {
                // Never started and not wanted: nothing to do, and in
                // particular no bus connection to open.
                return;
            }
            match start(events) {
                Ok(commands) => self.worker = Some(commands),
                Err(e) => {
                    tracing::warn!("the system tray is unavailable: {e:#}");
                    return;
                }
            }
        }
        self.send(Command::Enable(enabled));
    }

    /// Which icon theme names are resolved against, from the configuration.
    pub fn set_icon_theme(&self, theme: String) {
        self.send(Command::IconTheme(theme));
    }

    /// A click on an item, forwarded to whoever owns it.
    pub fn activate(&self, id: String, button: String, x: i32, y: i32) {
        self.send(Command::Activate { id, button, x, y });
    }

    /// A row of an open menu was chosen.
    pub fn menu_click(&self, id: String, item: i32) {
        self.send(Command::MenuClick { id, item });
    }

    /// An open menu was dismissed without a choice.
    pub fn menu_closed(&self, id: String) {
        self.send(Command::MenuClosed { id });
    }

    /// The wheel over an item.
    pub fn scroll(&self, id: String, delta: i32, orientation: String) {
        self.send(Command::Scroll {
            id,
            delta,
            orientation,
        });
    }

    fn send(&self, command: Command) {
        if let Some(worker) = self.worker.as_ref() {
            let _ = worker.send(command);
        }
    }
}

/// What the worker thread is asked to do, by the compositor and by the bus.
enum Command {
    /// An application registered itself, or re-registered after a restart.
    Register {
        service: String,
        path: String,
    },
    /// One item said something about itself changed — a new icon, a new
    /// title, a new status. Which of them it was is not worth tracking: the
    /// answer is to ask the item what it looks like now, and that is one round
    /// trip either way.
    Refresh {
        key: String,
    },
    /// A bus name went away. Every item owned by it goes with it, which is the
    /// only removal notice a crashing application gives.
    NameLost(String),
    Activate {
        id: String,
        button: String,
        x: i32,
        y: i32,
    },
    Scroll {
        id: String,
        delta: i32,
        orientation: String,
    },
    MenuClick {
        id: String,
        item: i32,
    },
    MenuClosed {
        id: String,
    },
    Enable(bool),
    IconTheme(String),
}

/// Claim the names, serve the watcher, and start the threads that feed it.
fn start(
    events: smithay::reexports::calloop::channel::Sender<Message>,
) -> anyhow::Result<mpsc::Sender<Command>> {
    let (commands, inbox) = mpsc::channel();

    // The watcher object answers on zbus's own executor and does no work: it
    // records nothing, and hands every registration to the worker, which is
    // the one place the item list lives.
    let connection = zbus::blocking::connection::Builder::session()?
        .serve_at(
            WATCHER_PATH,
            Watcher {
                commands: commands.clone(),
                items: std::sync::Arc::default(),
            },
        )?
        .build()?;

    // Signals from every tray item on the bus, on one match rule rather than a
    // subscription per item: an item that changes its icon does not send it,
    // it says that it changed, and the answer is the same refresh whichever
    // item and whichever signal it was.
    pump(
        connection.clone(),
        commands.clone(),
        format!("type='signal',interface='{ITEM}'"),
        |message, commands| {
            let header = message.header();
            let (Some(sender), Some(path)) = (header.sender(), header.path()) else {
                return;
            };
            let _ = commands.send(Command::Refresh {
                key: key(sender.as_str(), path.as_str()),
            });
        },
    )?;

    // And the only notice an application that dies gives.
    pump(
        connection.clone(),
        commands.clone(),
        "type='signal',sender='org.freedesktop.DBus',\
         interface='org.freedesktop.DBus',member='NameOwnerChanged'"
            .to_owned(),
        |message, commands| {
            let Ok((name, _old, new)) = message.body().deserialize::<(String, String, String)>()
            else {
                return;
            };
            // An empty new owner is the name being given up, which for a
            // unique name means the process is gone.
            if new.is_empty() {
                let _ = commands.send(Command::NameLost(name));
            }
        },
    )?;

    std::thread::Builder::new()
        .name("tray".to_owned())
        .spawn(move || Worker::new(connection, events).run(&inbox))?;

    Ok(commands)
}

/// One thread reading one match rule, turning messages into commands.
fn pump(
    connection: zbus::blocking::Connection,
    commands: mpsc::Sender<Command>,
    rule: String,
    handle: fn(&zbus::Message, &mpsc::Sender<Command>),
) -> anyhow::Result<()> {
    let rule = zbus::MatchRule::try_from(rule.as_str())?;
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)?;
    std::thread::Builder::new()
        .name("tray-signals".to_owned())
        .spawn(move || {
            for message in messages.flatten() {
                handle(&message, &commands);
            }
        })?;
    Ok(())
}

/// How an item is named in a message to the shell, and in the registry.
///
/// The bus name and the object path, joined. Both halves are needed — one
/// application may publish several items, and two applications may publish one
/// each at the same path — and neither is meaningful to the shell, which sends
/// it back as an opaque key.
fn key(service: &str, path: &str) -> String {
    format!("{service}{path}")
}

/// The object on the bus. Every method here is answered on zbus's executor,
/// and none of them does anything but record or forward.
struct Watcher {
    commands: mpsc::Sender<Command>,
    /// What `RegisteredStatusNotifierItems` answers with. Kept here rather
    /// than read back from the worker because it is a property an application
    /// may ask for at any moment, and the worker may be waiting on one that
    /// has stopped answering.
    items: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// An application announcing itself.
    ///
    /// The argument is either a bus name — in which case the item is at the
    /// well-known path — or an object path, in which case the item belongs to
    /// whoever sent the message. Both forms are in use: Qt sends the name,
    /// Ayatana's library sends the path, and an implementation that handles
    /// only one of them has a tray that works for half the desktop.
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let (service, path) = if service.starts_with('/') {
            (sender, service)
        } else {
            (service, DEFAULT_PATH.to_owned())
        };
        if service.is_empty() {
            return;
        }

        let key = key(&service, &path);
        if let Ok(mut items) = self.items.lock() {
            if !items.contains(&key) {
                items.push(key.clone());
            }
        }
        let _ = self.commands.send(Command::Register { service, path });
        let _ = Self::status_notifier_item_registered(&emitter, &key).await;
    }

    /// A host announcing itself. This compositor is the only host it needs,
    /// but the call is part of the interface and a program that gets an error
    /// from it may decide there is no tray at all.
    async fn register_status_notifier_host(
        &self,
        _service: String,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let _ = Self::status_notifier_host_registered(&emitter).await;
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    /// Always true while this is serving: the compositor draws the tray, so a
    /// host exists by construction. An item that reads false here draws a
    /// window of its own instead.
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

/// One tray item, as the worker tracks it.
struct Entry {
    service: String,
    path: String,
    /// The `com.canonical.dbusmenu` object this item points at, where it has
    /// one that answered a layout. `None` for an item with no menu, and for
    /// one whose menu could not be read — both mean the same thing here:
    /// asking for the menu is the application's job.
    menu: Option<String>,
    item: TrayItem,
}

/// The thread that owns the item list and does every call an item answers.
struct Worker {
    connection: zbus::blocking::Connection,
    events: smithay::reexports::calloop::channel::Sender<Message>,
    entries: Vec<Entry>,
    /// Icon names already resolved to a data URL, because resolving one walks
    /// the icon theme directories and an item that says its icon changed
    /// usually means its *status* changed and the icon with it — between two
    /// names, back and forth, for as long as the application runs.
    icons: HashMap<String, String>,
    theme: String,
    /// Whether the names are currently held.
    enabled: bool,
}

impl Worker {
    fn new(
        connection: zbus::blocking::Connection,
        events: smithay::reexports::calloop::channel::Sender<Message>,
    ) -> Self {
        Self {
            connection,
            events,
            entries: Vec::new(),
            icons: HashMap::new(),
            // What the configuration has not overridden. hicolor is searched
            // in either case; this is the theme searched before it.
            theme: "hicolor".to_owned(),
            enabled: false,
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Command>) {
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Enable(true) => self.claim(),
                Command::Enable(false) => self.release(),
                Command::IconTheme(theme) => {
                    if theme != self.theme {
                        self.theme = theme;
                        self.icons.clear();
                        for index in 0..self.entries.len() {
                            self.refresh_at(index);
                        }
                        self.publish();
                    }
                }
                // Everything below is a no-op while the tray is off. A
                // registration cannot arrive then — the name is not held — but
                // a signal from an item registered before it was turned off
                // can.
                _ if !self.enabled => {}
                Command::Register { service, path } => self.register(service, path),
                Command::Refresh { key } => {
                    if let Some(index) = self.index_of(&key) {
                        self.refresh_at(index);
                        self.publish();
                    }
                }
                Command::NameLost(name) => self.drop_owner(&name),
                Command::Activate { id, button, x, y } => {
                    // What the specification calls the two clicks, plus the
                    // menu. An item that says it *is* a menu gets its menu
                    // from the primary click, which is what the property means
                    // and what every other tray does with it.
                    let Some(entry) = self.index_of(&id).map(|i| &self.entries[i]) else {
                        return_missing(&id);
                        continue;
                    };
                    let wants_menu = button == "menu" || entry.item.is_menu;
                    // A menu this compositor can draw is drawn here; anything
                    // else is the application's own job, which is what
                    // `ContextMenu` asks it to do. The shell asks the same
                    // question for both, because which it is depends on the
                    // toolkit the application was written with and is not
                    // something a desktop should have an opinion about.
                    if wants_menu && entry.menu.is_some() {
                        self.open_menu(&id, x, y);
                        continue;
                    }
                    let method = match button.as_str() {
                        "secondary" => "SecondaryActivate",
                        _ if wants_menu => "ContextMenu",
                        _ => "Activate",
                    };
                    self.call(&id, method, &(x, y));
                }
                Command::MenuClick { id, item } => {
                    // `clicked` is the event a chosen row sends, and the
                    // timestamp is the specification's — zero where there is
                    // none to give, which is what a click routed through the
                    // shell has.
                    self.menu_event(&id, item, "clicked");
                }
                Command::MenuClosed { id } => {
                    // Closing is reported against the root, because that is
                    // what was opened. Applications rebuild their menu on
                    // this: one that is never told keeps serving a stale one.
                    self.menu_event(&id, 0, "closed");
                }
                Command::Scroll {
                    id,
                    delta,
                    orientation,
                } => {
                    let orientation = if orientation == "horizontal" {
                        "horizontal"
                    } else {
                        "vertical"
                    };
                    self.call(&id, "Scroll", &(delta, orientation));
                }
            }
        }
    }

    /// Ask for the names, and tell the session a host has appeared.
    ///
    /// Both names, and both matter. Applications look for the watcher; some of
    /// them then check that a host is registered before they will use it, and
    /// the host name is what that check reads. The name carries this process's
    /// id because the specification says a host names itself that way, and
    /// because two hosts on one session must not collide.
    fn claim(&mut self) {
        if self.enabled {
            return;
        }
        let host = format!("org.kde.StatusNotifierHost-{}", std::process::id());
        for name in [WATCHER_NAME.to_owned(), host] {
            // The compositor's own flags: it queues for a name and never takes
            // one. A KDE session or a GNOME extension that already draws a
            // tray knows more about that desktop than this does — and when it
            // exits, this gets the name rather than the session losing its
            // tray. See `crate::dbus::name_flags`.
            let reply = self
                .connection
                .request_name_with_flags(name.as_str(), crate::dbus::name_flags());
            crate::dbus::log_name_reply(&name, reply);
        }
        self.enabled = true;
    }

    /// Give the names back, and take the tray off the bar.
    ///
    /// The connection stays open. Its threads are blocked reading from the bus
    /// and cannot be asked to stop; releasing the names is what an application
    /// watching for a tray actually reacts to, and re-claiming them is one
    /// call if the configuration turns it back on.
    fn release(&mut self) {
        if !self.enabled {
            return;
        }
        let host = format!("org.kde.StatusNotifierHost-{}", std::process::id());
        for name in [WATCHER_NAME.to_owned(), host] {
            if let Err(e) = self.connection.release_name(name.as_str()) {
                tracing::warn!("could not release {name}: {e}");
            }
        }
        self.enabled = false;
        self.entries.clear();
        self.icons.clear();
        // An empty tray rather than a stale one: the shell draws what it was
        // last told, so a tray switched off in the configuration has to be
        // told it is empty or the icons stay on the bar until the shell
        // reloads.
        self.publish();
    }

    fn index_of(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.item.id == key)
    }

    fn register(&mut self, service: String, path: String) {
        let key = key(&service, &path);
        if let Some(index) = self.index_of(&key) {
            // A re-registration, which is what an application that restarted
            // its own item sends. Fetch what it looks like now rather than
            // adding it twice.
            self.refresh_at(index);
        } else {
            self.entries.push(Entry {
                service,
                path,
                menu: None,
                item: TrayItem {
                    id: key,
                    title: String::new(),
                    status: "active".to_owned(),
                    icon: String::new(),
                    tooltip: String::new(),
                    is_menu: false,
                    has_menu: false,
                },
            });
            let index = self.entries.len() - 1;
            self.refresh_at(index);
        }
        self.publish();
    }

    /// Everything owned by a bus name that has gone.
    ///
    /// Matched on the item's own service, which is the unique name for an item
    /// registered by path and whatever the application asked for otherwise —
    /// so a well-known name being released removes the item registered under
    /// it, and the unique name vanishing removes the rest.
    fn drop_owner(&mut self, name: &str) {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.service != name);
        if self.entries.len() != before {
            self.publish();
        }
    }

    /// Ask one item what it looks like now.
    ///
    /// Every property is optional here, including the ones the specification
    /// requires. Items in the wild leave out `Title`, `ToolTip` and
    /// `ItemIsMenu`, and answer errors for properties they do not implement; a
    /// fetch that gave up on the first error would drop icons that are
    /// perfectly well published.
    fn refresh_at(&mut self, index: usize) {
        let (service, path) = {
            let entry = &self.entries[index];
            (entry.service.clone(), entry.path.clone())
        };
        let Some(proxy) = self.proxy(&service, &path) else {
            return;
        };

        let get = |name: &str| -> Option<zvariant::OwnedValue> { proxy.get_property(name).ok() };
        let text =
            |name: &str| -> String { proxy.get_property::<String>(name).unwrap_or_default() };

        let status = text("Status").to_lowercase();
        let status = match status.as_str() {
            "passive" => "passive".to_owned(),
            "needsattention" | "needs-attention" => "needs-attention".to_owned(),
            // Anything else, including an item that answers nothing at all,
            // is shown. An icon nobody asked to hide is better than a program
            // with no way to be reached.
            _ => "active".to_owned(),
        };

        let id = text("Id");
        let title = {
            let title = text("Title");
            if title.is_empty() {
                id
            } else {
                title
            }
        };

        // An item asking for attention says so with a second icon, and the
        // whole point of the status is that this one is drawn instead.
        let attention = status == "needs-attention";
        let name_property = if attention {
            "AttentionIconName"
        } else {
            "IconName"
        };
        let pixmap_property = if attention {
            "AttentionIconPixmap"
        } else {
            "IconPixmap"
        };

        let theme_path = text("IconThemePath");
        let icon = self
            .icon_by_name(&text(name_property), &theme_path)
            .or_else(|| get(pixmap_property).and_then(|value| pixmap_url(&value)))
            // An item in attention that publishes no attention icon keeps the
            // one it had, rather than losing its icon at the moment it most
            // wants to be seen.
            .or_else(|| {
                attention
                    .then(|| {
                        self.icon_by_name(&text("IconName"), &theme_path)
                            .or_else(|| get("IconPixmap").and_then(|value| pixmap_url(&value)))
                    })
                    .flatten()
            })
            .unwrap_or_default();

        let tooltip = get("ToolTip")
            .map(|value| tooltip(&value))
            .unwrap_or_default();
        let is_menu = proxy.get_property::<bool>("ItemIsMenu").unwrap_or(false);

        // Where the item keeps its menu, if it keeps one here at all. The
        // property is an object path and "/" is what an item with no menu
        // publishes — a valid path pointing at nothing, which every toolkit
        // sends rather than leaving the property out.
        let menu = proxy
            .get_property::<zvariant::OwnedObjectPath>("Menu")
            .ok()
            .map(|path| path.as_str().to_owned())
            .filter(|path| path != "/");

        let entry = &mut self.entries[index];
        entry.item.title = title;
        entry.item.status = status;
        entry.item.icon = icon;
        entry.item.tooltip = tooltip;
        entry.item.is_menu = is_menu;
        entry.item.has_menu = menu.is_some();
        entry.menu = menu;
    }

    /// Fetch one item's menu and hand it to the shell.
    ///
    /// Two calls, in the order the specification wants them. `AboutToShow`
    /// first, because a menu is usually built when it is asked for — an
    /// application that populates lazily answers an empty layout to anything
    /// that skips it, which looks like a menu with nothing in it. Its answer
    /// says whether the layout changed and is ignored: the layout is fetched
    /// either way, and an item that does not implement the call answers an
    /// error that means nothing about whether it has a menu.
    ///
    /// The whole tree comes back in one call. A menu is small, the shell draws
    /// it in one pass, and a round trip per submenu would be a menu that opens
    /// in stages while the compositor is trying to hold a frame.
    fn open_menu(&mut self, id: &str, x: i32, y: i32) {
        let Some(index) = self.index_of(id) else {
            return_missing(id);
            return;
        };
        let (service, menu) = {
            let entry = &self.entries[index];
            (entry.service.clone(), entry.menu.clone())
        };
        let Some(menu) = menu else { return };
        let Some(proxy) = self.menu_proxy(&service, &menu) else {
            return;
        };

        let _ = proxy.call::<_, _, bool>("AboutToShow", &(0i32));

        // Depth -1 is the whole tree, and the empty list of property names
        // means every property rather than none — both are the specification's
        // spelling, and both read backwards.
        let layout: zbus::Result<(u32, MenuNode)> =
            proxy.call("GetLayout", &(0i32, -1i32, Vec::<&str>::new()));
        let items = match layout {
            Ok((_revision, root)) => self.menu_items(&root.2),
            Err(e) => {
                // Falling back rather than showing nothing: an item whose menu
                // object does not answer may still draw its own window, which
                // is what every tray does with an item like that.
                tracing::debug!("{id}: no menu layout: {e}");
                self.call(id, "ContextMenu", &(x, y));
                return;
            }
        };

        // Told it is open, as the specification asks, so an application that
        // tracks its own menu knows one is on screen.
        let _ = proxy.call_noreply(
            "Event",
            &(0i32, "opened", zvariant::Value::from(0i32), 0u32),
        );

        let _ = self.events.send(Message::Menu {
            id: id.to_owned(),
            x,
            y,
            items,
        });
    }

    /// Tell an application what happened to its menu.
    fn menu_event(&self, id: &str, item: i32, event: &str) {
        let Some(index) = self.index_of(id) else {
            return_missing(id);
            return;
        };
        let (service, menu) = {
            let entry = &self.entries[index];
            (entry.service.clone(), entry.menu.clone())
        };
        let Some(menu) = menu else { return };
        let Some(proxy) = self.menu_proxy(&service, &menu) else {
            return;
        };
        // Zero for the timestamp: the specification wants the moment the user
        // acted, and what this has is a message from a page that has no clock
        // the application shares. Every implementation sends zero here.
        if let Err(e) =
            proxy.call_noreply("Event", &(item, event, zvariant::Value::from(0i32), 0u32))
        {
            tracing::debug!("{id}: menu {event} on {item} failed: {e}");
        }
    }

    /// One level of a menu, and everything under it.
    fn menu_items(
        &mut self,
        children: &[zvariant::OwnedValue],
    ) -> Vec<viewport_ipc::event::TrayMenuItem> {
        children
            .iter()
            .filter_map(|child| MenuNode::try_from(child.clone()).ok())
            .filter_map(|node| self.menu_item(&node))
            .collect()
    }

    /// One row, or nothing where the application asked for it not to be shown.
    fn menu_item(&mut self, node: &MenuNode) -> Option<viewport_ipc::event::TrayMenuItem> {
        let props = &node.1;
        let text = |name: &str| -> String {
            props
                .get(name)
                .and_then(|value| <&str>::try_from(value).ok())
                .unwrap_or_default()
                .to_owned()
        };
        let flag = |name: &str, default: bool| -> bool {
            props
                .get(name)
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(default)
        };

        // Both default to true, which is the specification's way of saying
        // that the common row carries no properties at all.
        if !flag("visible", true) {
            return None;
        }

        let kind = match text("type").as_str() {
            "separator" => "separator".to_owned(),
            _ => "standard".to_owned(),
        };

        // The icon: a theme name, or a PNG the row carries itself. Menus use
        // the second far more than tray items do, because a row's icon is
        // usually part of the application rather than part of the desktop.
        let icon = self
            .icon_by_name(&text("icon-name"), "")
            .or_else(|| {
                props
                    .get("icon-data")
                    .and_then(|value| <Vec<u8>>::try_from(value.clone()).ok())
                    .and_then(|bytes| crate::icon::png_data_url(&bytes))
            })
            .unwrap_or_default();

        let toggle = match text("toggle-type").as_str() {
            "checkmark" => "checkmark".to_owned(),
            "radio" => "radio".to_owned(),
            _ => String::new(),
        };
        // Three states, not two: 1 is on, 0 is off and -1 is "this row does
        // not say", which is drawn as off.
        let checked = props
            .get("toggle-state")
            .and_then(|value| i32::try_from(value).ok())
            .is_some_and(|state| state == 1);

        Some(viewport_ipc::event::TrayMenuItem {
            id: node.0,
            label: strip_mnemonics(&text("label")),
            kind,
            enabled: flag("enabled", true),
            toggle,
            checked,
            icon,
            children: self.menu_items(&node.2),
        })
    }

    /// A proxy onto one item's menu object.
    fn menu_proxy(&self, service: &str, path: &str) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(service.to_owned())
            .ok()?
            .path(path.to_owned())
            .ok()?
            .interface(MENU)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    }

    /// An icon name resolved through the themes, cached by name and by the
    /// item's own theme directory.
    fn icon_by_name(&mut self, name: &str, theme_path: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        let cache_key = format!("{theme_path}\u{1}{name}");
        if let Some(url) = self.icons.get(&cache_key) {
            return Some(url.clone());
        }
        let path = crate::icon::lookup(
            name,
            (!theme_path.is_empty()).then_some(theme_path),
            &self.theme,
            ICON_SIZE,
        )?;
        let url = crate::icon::data_url(&path)?;
        self.icons.insert(cache_key, url.clone());
        Some(url)
    }

    /// Call a method on an item, ignoring what it answers.
    ///
    /// A tray item's methods return nothing, and an application that has
    /// stopped answering the bus must not stall the tray: the reply is not
    /// waited for at all.
    fn call<B>(&self, id: &str, method: &str, body: &B)
    where
        B: serde::Serialize + zvariant::DynamicType,
    {
        let Some(index) = self.index_of(id) else {
            return_missing(id);
            return;
        };
        let entry = &self.entries[index];
        let Some(proxy) = self.proxy(&entry.service.clone(), &entry.path.clone()) else {
            return;
        };
        if let Err(e) = proxy.call_noreply(method, body) {
            // Not a warning. Items answer `UnknownMethod` for the calls they
            // do not implement — `SecondaryActivate` especially — and a middle
            // click on one of those is not a fault.
            tracing::debug!("{id}: {method} failed: {e}");
        }
    }

    /// A proxy onto one item, with property caching off.
    ///
    /// Caching would mean a subscription per item and a `PropertiesChanged`
    /// that most items never send: this specification has its own signals for
    /// what changed, which is what the pump above listens to. A cached proxy
    /// here would be a tray that updates only for the toolkits that send both.
    fn proxy(&self, service: &str, path: &str) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(service.to_owned())
            .ok()?
            .path(path.to_owned())
            .ok()?
            .interface(ITEM)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    }

    fn publish(&self) {
        let items = self.entries.iter().map(|e| e.item.clone()).collect();
        let _ = self.events.send(Message::Items(items));
    }
}

/// One node of a menu layout: an id, its properties, and its children.
///
/// `GetLayout` answers `(ia{sv}av)` — and the children are variants, which is
/// what makes the recursion fall out: every level below the root converts from
/// an `OwnedValue` into this same type. Only the root arrives as a bare
/// structure, which is why it is named here at all.
type MenuNode = (
    i32,
    HashMap<String, zvariant::OwnedValue>,
    Vec<zvariant::OwnedValue>,
);

/// A label with its keyboard mnemonic taken out.
///
/// Menu labels carry the underline marker the toolkit would have drawn —
/// `_Quit`, or `&Quit` in the Qt spelling — and a shell that draws the string
/// as it arrives shows the underscore. Doubled means a literal one, which is
/// how a program with an underscore in a filename spells it.
fn strip_mnemonics(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == '_' || c == '&') && chars.peek() == Some(&c) {
            out.push(c);
            chars.next();
        } else if c == '_' || c == '&' {
            // The marker itself: dropped, and the letter after it kept.
        } else {
            out.push(c);
        }
    }
    out
}

/// A click on an item that has already gone is a race, not a fault: the shell
/// draws what it was last told and an application may exit between the paint
/// and the press.
fn return_missing(id: &str) {
    tracing::debug!("tray item {id} is gone");
}

/// The tooltip, which is a struct of four fields and whose middle two nobody
/// uses.
///
/// `(icon name, icon pixmaps, title, body)`. The title and the body are joined
/// with a newline because that is what they are: a heading and the text under
/// it, and a shell that gets one string can style the two halves apart if it
/// wants to.
fn tooltip(value: &zvariant::OwnedValue) -> String {
    let Ok((_, _, title, body)) =
        <(String, Vec<(i32, i32, Vec<u8>)>, String, String)>::try_from(value.clone())
    else {
        return String::new();
    };
    match (title.is_empty(), body.is_empty()) {
        (true, true) => String::new(),
        (false, true) => title,
        (true, false) => body,
        (false, false) => format!("{title}\n{body}"),
    }
}

/// The pixmaps an item published, as a data URL.
fn pixmap_url(value: &zvariant::OwnedValue) -> Option<String> {
    let raw = <Vec<(i32, i32, Vec<u8>)>>::try_from(value.clone()).ok()?;
    let pixmaps: Vec<crate::icon::Pixmap> = raw
        .into_iter()
        .map(|(width, height, argb)| crate::icon::Pixmap {
            width,
            height,
            argb,
        })
        .collect();
    crate::icon::pixmap_url(&pixmaps, ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is both halves, because neither is unique on its own: one
    /// application may publish several items, and two may publish one each at
    /// the same well-known path.
    #[test]
    fn an_item_is_named_by_its_owner_and_its_path() {
        assert_eq!(
            key(":1.42", "/StatusNotifierItem"),
            ":1.42/StatusNotifierItem"
        );
        assert_ne!(
            key(":1.42", "/org/ayatana/NotificationItem/a"),
            key(":1.42", "/org/ayatana/NotificationItem/b")
        );
    }

    /// A label arrives with the underline marker the toolkit would have
    /// drawn. Both spellings are in use — GTK writes `_Quit` and Qt `&Quit` —
    /// and a doubled one is a literal character rather than a marker.
    #[test]
    fn a_label_loses_its_mnemonic_and_keeps_its_letters() {
        assert_eq!(strip_mnemonics("_Quit"), "Quit");
        assert_eq!(strip_mnemonics("&Quit"), "Quit");
        assert_eq!(strip_mnemonics("Save _As…"), "Save As…");
        assert_eq!(strip_mnemonics("my__file"), "my_file");
        assert_eq!(strip_mnemonics("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(strip_mnemonics("Quit"), "Quit");
    }

    /// A tooltip is four fields and two of them are the ones with text in.
    #[test]
    fn a_tooltip_is_its_title_and_its_body() {
        let value = |title: &str, body: &str| {
            zvariant::OwnedValue::try_from(zvariant::Value::from((
                String::new(),
                Vec::<(i32, i32, Vec<u8>)>::new(),
                title.to_owned(),
                body.to_owned(),
            )))
            .expect("a tooltip")
        };
        assert_eq!(
            tooltip(&value("Syncing", "3 files left")),
            "Syncing\n3 files left"
        );
        assert_eq!(tooltip(&value("Syncing", "")), "Syncing");
        assert_eq!(tooltip(&value("", "3 files left")), "3 files left");
        assert_eq!(tooltip(&value("", "")), "");
    }

    /// A tooltip of some other shape is nothing rather than an error: this is
    /// a value from an arbitrary program, and an item that publishes a
    /// malformed one still has an icon worth drawing.
    #[test]
    fn a_malformed_tooltip_is_no_tooltip() {
        let value = zvariant::OwnedValue::try_from(zvariant::Value::from(1u32)).expect("a value");
        assert_eq!(tooltip(&value), "");
    }
}
