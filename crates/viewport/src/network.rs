// SPDX-License-Identifier: GPL-3.0-or-later
//
// The wireless radio: what it can see, and joining one of them.
//
// The bar has always reported link throughput, which says a network is being
// used and nothing about which one or how to get on it. NetworkManager is the
// thing that knows: it is on the system bus, it holds the saved connections
// and the secrets for them, and every other desktop's applet is a client of
// it. So this is a client of it too, rather than a second thing with an
// opinion about networking — a compositor that drove wpa_supplicant directly
// would be fighting whatever the distribution already started.
//
// A thread of its own with a channel back, like MPRIS and UPower. The reason
// is the same and slightly worse here: reading the access point list is four
// property reads per access point, on a bus, and a coffee shop has forty of
// them. A compositor that did that on its own loop would drop a second of
// frames every time somebody walked past a router.
//
// Idle until the shell asks. Nothing here runs until a picker opens: the
// worker is started by the first request and stops reading the list the moment
// the picker says it has gone away. That matters more than it does for the
// battery — a scan is the radio transmitting, and a scan nobody is looking at
// is a battery cost with nothing on screen to show for it.

use std::collections::HashMap;
use std::sync::mpsc;

use viewport_ipc::event::{AccessPoint, NetworkSnapshot};

/// The bus name, and the manager object under it. Named once: every proxy
/// below is built against the same daemon and a typo in one of them is a
/// feature that quietly does nothing.
const NM: &str = "org.freedesktop.NetworkManager";
const MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";

/// `NMDeviceType`, of which two matter here: a radio to join networks with,
/// and a cable that explains why the machine is online without one.
const DEVICE_ETHERNET: u32 = 1;
const DEVICE_WIFI: u32 = 2;

/// `NM_DEVICE_STATE_ACTIVATED`. The states below it are stages of getting
/// there and the ones above are stages of coming apart; only this one means a
/// device is carrying traffic.
const DEVICE_ACTIVATED: u32 = 100;

/// `NM802_11ApFlags` and `NM802_11ApSecurityFlags`, which together are the
/// only way to tell what an access point wants.
///
/// The privacy bit alone — set, with neither WPA nor RSN saying anything — is
/// WEP, because that is the only thing it could be: WPA and WPA2 both
/// advertise a key management scheme, and an open network sets no bit at all.
const AP_PRIVACY: u32 = 0x1;
const KEY_MGMT_PSK: u32 = 0x100;
const KEY_MGMT_802_1X: u32 = 0x200;
const KEY_MGMT_SAE: u32 = 0x400;

/// `NMState`, as the whole machine rather than one device. Only the four
/// answers a picker draws differently are named.
const STATE_CONNECTING: u32 = 40;
const STATE_CONNECTED_LOCAL: u32 = 50;
const STATE_CONNECTED_GLOBAL: u32 = 70;

/// What the thread sends the compositor.
#[derive(Debug)]
pub enum Message {
    Snapshot(NetworkSnapshot),
}

/// The half the compositor keeps.
///
/// No `set_enabled` from the config, unlike [`crate::power::Power`]: there is
/// no widget whose presence decides whether this is wanted, and the answer to
/// "does anybody want the network list" is only ever known by the picker that
/// is drawing it. So the worker is started by the first request that arrives
/// and the shell says when to stop reading.
#[derive(Default)]
pub struct Network {
    worker: Option<mpsc::Sender<Command>>,
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
    /// Whether the worker has already been started and failed. Without it, a
    /// picker opened on a machine with no NetworkManager would try to connect
    /// to the system bus on every keypress.
    unavailable: bool,
}

impl Network {
    /// Where updates go. Called once, when the event loop has a source.
    pub fn attach(&mut self, events: smithay::reexports::calloop::channel::Sender<Message>) {
        self.events = Some(events);
    }

    /// Watch, or stop watching. The picker opening and closing.
    ///
    /// Watching means both halves of what a picker wants: the list is read and
    /// sent now, a scan is asked for so that it is read again in a moment with
    /// whatever the radio found, and NetworkManager's own signals keep it up
    /// to date until the picker says it has gone.
    pub fn watch(&mut self, on: bool) {
        // Not started on the way out. A shell that closes a picker it never
        // opened — a reload while one was up, which sends the close and not the
        // open — should not be what stands the worker up.
        if !on && self.worker.is_none() {
            return;
        }
        self.send(Command::Watch(on));
    }

    /// Join a network, with a passphrase for one that is not already known.
    pub fn connect(&mut self, ssid: String, passphrase: Option<String>) {
        self.send(Command::Connect { ssid, passphrase });
    }

    /// Leave the network in use.
    pub fn disconnect(&mut self) {
        self.send(Command::Disconnect);
    }

    /// Switch the radio on or off; `None` toggles.
    pub fn radio(&mut self, enabled: Option<bool>) {
        self.send(Command::Radio(enabled));
    }

    /// Hand a command to the worker, starting it if this is the first one.
    ///
    /// A worker that has gone — the thread ended because the channel closed —
    /// is not restarted. There is one way for that to happen and it is the
    /// compositor shutting down.
    fn send(&mut self, command: Command) {
        if self.worker.is_none() {
            if self.unavailable {
                return;
            }
            let Some(events) = self.events.clone() else {
                return;
            };
            match start(events) {
                Ok(worker) => self.worker = Some(worker),
                Err(e) => {
                    tracing::warn!("network: NetworkManager is unavailable: {e:#}");
                    self.unavailable = true;
                    // Said rather than left silent: a picker with no answer at
                    // all draws an empty list, which reads as "no networks"
                    // rather than as "nothing to ask".
                    if let Some(events) = self.events.as_ref() {
                        let _ = events.send(Message::Snapshot(NetworkSnapshot::default()));
                    }
                    return;
                }
            }
        }
        if let Some(worker) = self.worker.as_ref() {
            let _ = worker.send(command);
        }
    }
}

enum Command {
    /// Read everything and send it if it changed.
    Refresh,
    Watch(bool),
    Connect {
        ssid: String,
        passphrase: Option<String>,
    },
    Disconnect,
    Radio(Option<bool>),
}

fn start(
    events: smithay::reexports::calloop::channel::Sender<Message>,
) -> anyhow::Result<mpsc::Sender<Command>> {
    let (commands, inbox) = mpsc::channel();
    let connection = zbus::blocking::Connection::system()?;

    // One match rule for the whole daemon rather than one per object. The
    // manager's properties, each device's, each access point's and each saved
    // connection's all arrive on it, and every one of them is a reason to read
    // again: a strength that moved, a device that associated, a connection
    // somebody added from nmcli. Over-receiving costs a read that finds
    // nothing changed and is dropped by the comparison in `refresh`; missing
    // one is a picker that says a network is still there after the radio was
    // switched off.
    pump(
        connection.clone(),
        commands.clone(),
        format!("type='signal',interface='org.freedesktop.DBus.Properties',sender='{NM}'"),
    )?;

    std::thread::Builder::new()
        .name("network".to_owned())
        .spawn(move || Worker::new(connection, events).run(&inbox))?;
    Ok(commands)
}

/// A thread that turns one match rule's traffic into `Refresh`.
///
/// Its own thread rather than a dispatch on the worker's, because the worker
/// spends its time blocked on method calls that can take seconds — associating
/// with an access point is one — and a signal that arrives during one of those
/// must not be dropped.
fn pump(
    connection: zbus::blocking::Connection,
    commands: mpsc::Sender<Command>,
    rule: String,
) -> anyhow::Result<()> {
    let rule = zbus::MatchRule::try_from(rule.as_str())?;
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)?;
    std::thread::Builder::new()
        .name("network-signals".to_owned())
        .spawn(move || {
            for _ in messages.flatten() {
                if commands.send(Command::Refresh).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

struct Worker {
    connection: zbus::blocking::Connection,
    events: smithay::reexports::calloop::channel::Sender<Message>,
    last: NetworkSnapshot,
    /// Whether a picker is up. While it is not, a signal is still received —
    /// the match rule stays — but nothing is read and nothing is sent.
    watching: bool,
    /// What the last attempt to join something said, carried on the next
    /// snapshot. Kept here rather than read back off the daemon because
    /// NetworkManager reports a refused passphrase as an activation that
    /// ended, with the reason on an object that is gone by the time anything
    /// could ask.
    error: Option<String>,
    /// What `LastScan` read when a scan was asked for, or `None` when this
    /// compositor has not asked for one since the picker opened. See
    /// [`Worker::scanning`].
    scan_asked: Option<i64>,
}

impl Worker {
    fn new(
        connection: zbus::blocking::Connection,
        events: smithay::reexports::calloop::channel::Sender<Message>,
    ) -> Self {
        Self {
            connection,
            events,
            last: NetworkSnapshot::default(),
            watching: false,
            error: None,
            scan_asked: None,
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Command>) {
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Watch(on) => {
                    self.watching = on;
                    if on {
                        // The list as it stands, then a scan so that it is
                        // read again with whatever the radio finds. Both,
                        // because a scan takes a couple of seconds and a
                        // picker that showed nothing until it finished would
                        // look broken every time it opened.
                        self.refresh();
                        self.scan();
                    } else {
                        // Forgotten, so that the next open sends a snapshot
                        // rather than comparing against a list from an hour
                        // ago and deciding nothing changed.
                        self.last = NetworkSnapshot::default();
                        self.error = None;
                        self.scan_asked = None;
                    }
                }
                _ if !self.watching => {}
                Command::Refresh => self.refresh(),
                Command::Connect { ssid, passphrase } => {
                    self.error = self.connect(&ssid, passphrase.as_deref()).err();
                    if let Some(e) = self.error.as_ref() {
                        tracing::info!("network: joining {ssid:?} failed: {e}");
                    }
                    self.refresh();
                }
                Command::Disconnect => {
                    self.error = self.disconnect().err();
                    self.refresh();
                }
                Command::Radio(enabled) => {
                    self.error = self.set_radio(enabled).err();
                    self.refresh();
                }
            }
        }
    }

    /// Read everything and send it, if it is not what was sent last.
    fn refresh(&mut self) {
        let snapshot = self.read();
        if snapshot == self.last {
            return;
        }
        self.last = snapshot.clone();
        let _ = self.events.send(Message::Snapshot(snapshot));
    }

    fn read(&self) -> NetworkSnapshot {
        let Some(manager) = self.proxy(MANAGER_PATH, NM) else {
            return NetworkSnapshot::default();
        };

        let mut snapshot = NetworkSnapshot {
            available: true,
            enabled: manager
                .get_property::<bool>("WirelessEnabled")
                .unwrap_or(false),
            state: match manager.get_property::<u32>("State").unwrap_or(0) {
                STATE_CONNECTED_LOCAL..=STATE_CONNECTED_GLOBAL => "connected",
                STATE_CONNECTING => "connecting",
                0 => "unknown",
                _ => "disconnected",
            }
            .to_owned(),
            error: self.error.clone(),
            ..NetworkSnapshot::default()
        };

        let devices: Vec<zvariant::OwnedObjectPath> =
            manager.get_property("Devices").unwrap_or_default();
        let known = self.known_networks();

        for path in &devices {
            let Some(device) = self.proxy(path.as_str(), "org.freedesktop.NetworkManager.Device")
            else {
                continue;
            };
            let kind = device.get_property::<u32>("DeviceType").unwrap_or(0);
            let state = device.get_property::<u32>("State").unwrap_or(0);
            if kind == DEVICE_ETHERNET && state == DEVICE_ACTIVATED {
                snapshot.wired = true;
                continue;
            }
            if kind != DEVICE_WIFI {
                continue;
            }
            // The first wireless device and no other. A machine with two is a
            // laptop with a USB adapter plugged in, and a picker that listed
            // both radios' views of the same room would offer every network
            // twice with no way to tell which row was which.
            if snapshot.wireless {
                continue;
            }
            snapshot.wireless = true;
            self.read_wireless(path, &known, &mut snapshot);
        }

        snapshot
    }

    /// One wireless device: what it can see, and what it is on.
    fn read_wireless(
        &self,
        path: &zvariant::OwnedObjectPath,
        known: &HashMap<String, zvariant::OwnedObjectPath>,
        snapshot: &mut NetworkSnapshot,
    ) {
        let Some(wireless) = self.proxy(
            path.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        ) else {
            return;
        };

        // The path of the access point in use, so the row for it can say so.
        // `"/"` is the object path that means none, which is what this
        // property is while the radio is associating or off.
        let active: String = wireless
            .get_property::<zvariant::OwnedObjectPath>("ActiveAccessPoint")
            .map(|path| path.as_str().to_owned())
            .unwrap_or_default();

        let points: Vec<zvariant::OwnedObjectPath> =
            wireless.get_property("AccessPoints").unwrap_or_default();

        // Merged by name rather than listed by radio. A house with a mesh
        // publishes the same SSID from three access points and joining it is
        // joining the network; three rows that all say "kitchen" is a choice
        // that is not one. The strongest of them is the one whose strength the
        // row shows, because that is the one the radio would use.
        let mut merged: Vec<AccessPoint> = Vec::new();
        for point in &points {
            let Some(proxy) =
                self.proxy(point.as_str(), "org.freedesktop.NetworkManager.AccessPoint")
            else {
                continue;
            };
            let raw: Vec<u8> = proxy.get_property("Ssid").unwrap_or_default();
            // A hidden network publishes an empty SSID and a non-UTF-8 one
            // cannot be written into JSON, drawn in a list or typed back.
            // Neither is a row somebody could act on.
            let Ok(ssid) = String::from_utf8(raw) else {
                continue;
            };
            if ssid.is_empty() {
                continue;
            }

            let strength = proxy.get_property::<u8>("Strength").unwrap_or(0);
            let security = security_of(
                proxy.get_property::<u32>("Flags").unwrap_or(0),
                proxy.get_property::<u32>("WpaFlags").unwrap_or(0),
                proxy.get_property::<u32>("RsnFlags").unwrap_or(0),
            );
            let is_active = !active.is_empty() && active != "/" && active == point.as_str();

            match merged.iter_mut().find(|row| row.ssid == ssid) {
                Some(row) => {
                    row.strength = row.strength.max(strength);
                    row.active |= is_active;
                }
                None => merged.push(AccessPoint {
                    known: known.contains_key(&ssid),
                    ssid,
                    strength,
                    security: security.to_owned(),
                    active: is_active,
                }),
            }
        }

        // Strongest first, and the one in use above everything: the row a
        // picker opens on is the network it is already on, and the rest are in
        // the order somebody would try them.
        merged.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then(b.strength.cmp(&a.strength))
                .then(a.ssid.cmp(&b.ssid))
        });

        snapshot.ssid = merged
            .iter()
            .find(|row| row.active)
            .map(|row| row.ssid.clone());
        snapshot.access_points = merged;
        snapshot.scanning = self.scanning(&wireless);
    }

    /// Whether the scan this compositor asked for has finished.
    ///
    /// There is no "scanning" property, and `LastScan` — the one thing the
    /// interface offers — is a `CLOCK_BOOTTIME` timestamp in milliseconds,
    /// which this process cannot read: `Instant` is `CLOCK_MONOTONIC` and the
    /// two differ by however long the machine has been suspended. So the
    /// comparison is against the daemon's own previous answer rather than
    /// against a clock. `RequestScan` records what `LastScan` was when it was
    /// asked, and the scan is running until that number moves.
    ///
    /// It only ever says a word on the picker, so the one case it gets wrong —
    /// a scan somebody else asked for, which this never saw begin — costs
    /// nothing but the word.
    fn scanning(&self, wireless: &zbus::blocking::Proxy<'static>) -> bool {
        let Some(asked) = self.scan_asked else {
            return false;
        };
        wireless.get_property::<i64>("LastScan").unwrap_or(-1) == asked
    }

    /// Every saved wireless connection, by the network it is for.
    ///
    /// This is what tells a row that can be joined with one click from one
    /// that has to ask for a passphrase first, and it is also how a known
    /// network is activated: the connection object is what
    /// `ActivateConnection` takes.
    fn known_networks(&self) -> HashMap<String, zvariant::OwnedObjectPath> {
        let mut known = HashMap::new();
        let Some(settings) = self.proxy(SETTINGS_PATH, "org.freedesktop.NetworkManager.Settings")
        else {
            return known;
        };
        let connections: Vec<zvariant::OwnedObjectPath> =
            settings.call("ListConnections", &()).unwrap_or_default();
        for path in connections {
            let Some(proxy) = self.proxy(
                path.as_str(),
                "org.freedesktop.NetworkManager.Settings.Connection",
            ) else {
                continue;
            };
            type Settings = HashMap<String, HashMap<String, zvariant::OwnedValue>>;
            let Ok(sections) = proxy.call::<&str, (), Settings>("GetSettings", &()) else {
                continue;
            };
            let Some(wireless) = sections.get("802-11-wireless") else {
                continue;
            };
            let Some(raw) = wireless.get("ssid").and_then(as_bytes) else {
                continue;
            };
            if let Ok(ssid) = String::from_utf8(raw) {
                known.insert(ssid, path);
            }
        }
        known
    }

    /// Ask the radio to look around.
    ///
    /// The answer is not waited for and there is nothing to wait for: the
    /// device publishes what it found as it finds it, and every one of those
    /// is a `PropertiesChanged` that comes back here as a refresh. A scan
    /// already in progress is refused with `AlreadyScanning`, which is not
    /// worth reporting — it means the thing that was asked for is happening.
    fn scan(&mut self) {
        let Some(path) = self.wireless_device() else {
            return;
        };
        let Some(wireless) = self.proxy(
            path.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        ) else {
            return;
        };
        // Read before asking, so that "the number has not moved" is a question
        // about this scan and not about one that finished a minute ago.
        let before = wireless.get_property::<i64>("LastScan").unwrap_or(-1);
        let options: HashMap<&str, zvariant::Value> = HashMap::new();
        match wireless
            .call::<&str, (HashMap<&str, zvariant::Value>,), ()>("RequestScan", &(options,))
        {
            Ok(()) => self.scan_asked = Some(before),
            // `AlreadyScanning` is not a failure — what was asked for is
            // happening — and neither is a radio that is switched off, which
            // is a picker that already says so.
            Err(e) => tracing::debug!("network: a scan was refused: {e}"),
        }
    }

    /// The first wireless device, which is the one everything acts on.
    fn wireless_device(&self) -> Option<zvariant::OwnedObjectPath> {
        let manager = self.proxy(MANAGER_PATH, NM)?;
        let devices: Vec<zvariant::OwnedObjectPath> =
            manager.get_property("Devices").unwrap_or_default();
        devices.into_iter().find(|path| {
            self.proxy(path.as_str(), "org.freedesktop.NetworkManager.Device")
                .and_then(|device| device.get_property::<u32>("DeviceType").ok())
                == Some(DEVICE_WIFI)
        })
    }

    /// Join a network, one of the two ways there are to.
    ///
    /// A network NetworkManager already has a connection for is activated:
    /// that is the saved passphrase, the saved 802.1X identity and whatever
    /// else somebody configured with nmcli or another desktop's applet, and
    /// making a second connection beside it would leave two entries for one
    /// network and only one of them right.
    ///
    /// A network it does not know is `AddAndActivateConnection`, which is what
    /// `nmcli device wifi connect` does: a connection is created from the
    /// passphrase and activated in one call, and NetworkManager keeps the
    /// secret in the store the rest of the desktop already reads. Nothing here
    /// writes it down — the string arrives in a request, goes out in a method
    /// call and is dropped with the command.
    fn connect(&self, ssid: &str, passphrase: Option<&str>) -> Result<(), String> {
        let manager = self
            .proxy(MANAGER_PATH, NM)
            .ok_or_else(|| "NetworkManager is not answering".to_owned())?;
        let device = self
            .wireless_device()
            .ok_or_else(|| "there is no wireless device".to_owned())?;

        if let Some(connection) = self.known_networks().get(ssid) {
            // `"/"` for the specific object: any access point publishing this
            // network will do, which is the whole point of a mesh.
            let root = zvariant::ObjectPath::try_from("/").expect("\"/\" is an object path");
            return manager
                .call::<&str, _, zvariant::OwnedObjectPath>(
                    "ActivateConnection",
                    &(connection.as_ref(), device.as_ref(), root),
                )
                .map(|_| ())
                .map_err(complaint);
        }

        let passphrase = passphrase.unwrap_or_default();
        let settings = new_connection(ssid, passphrase);
        let root = zvariant::ObjectPath::try_from("/").expect("\"/\" is an object path");
        manager
            .call::<&str, _, (zvariant::OwnedObjectPath, zvariant::OwnedObjectPath)>(
                "AddAndActivateConnection",
                &(settings, device.as_ref(), root),
            )
            .map(|_| ())
            .map_err(complaint)
    }

    /// Leave the network in use.
    ///
    /// The device is disconnected rather than the connection deleted or
    /// deactivated by name: what somebody means by leaving a network is that
    /// the radio should stop being on it, and `Disconnect` is the one call
    /// that says exactly that whatever is currently active.
    fn disconnect(&self) -> Result<(), String> {
        let device = self
            .wireless_device()
            .ok_or_else(|| "there is no wireless device".to_owned())?;
        let proxy = self
            .proxy(device.as_str(), "org.freedesktop.NetworkManager.Device")
            .ok_or_else(|| "NetworkManager is not answering".to_owned())?;
        proxy
            .call::<&str, (), ()>("Disconnect", &())
            .map_err(complaint)
    }

    /// Switch the radio on or off. `None` is a toggle, read from what the
    /// daemon says now rather than from the last snapshot — an rfkill switch
    /// flicked on the side of a laptop changes this without anything on screen
    /// hearing about it first.
    fn set_radio(&self, enabled: Option<bool>) -> Result<(), String> {
        let manager = self
            .proxy(MANAGER_PATH, NM)
            .ok_or_else(|| "NetworkManager is not answering".to_owned())?;
        let enabled = match enabled {
            Some(enabled) => enabled,
            None => !manager
                .get_property::<bool>("WirelessEnabled")
                .unwrap_or(false),
        };
        manager
            .set_property("WirelessEnabled", enabled)
            .map_err(complaint)
    }

    fn proxy(&self, path: &str, interface: &str) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(NM.to_owned())
            .ok()?
            .path(path.to_owned())
            .ok()?
            .interface(interface.to_owned())
            .ok()?
            // Uncached for the reason power.rs gives: these properties are
            // read once each and thrown away, and a cache is a second
            // subscription per object — one per access point, here.
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    }
}

/// The connection document `AddAndActivateConnection` takes for a network
/// nobody has joined before.
///
/// Deliberately minimal: an id, the network's name, and how to authenticate.
/// Everything else — DHCP, the route metric, whether to connect automatically
/// — is left for NetworkManager to default, because its defaults are what the
/// rest of the desktop's tools produce and a connection that looks different
/// from the ones nmcli writes is one somebody has to debug later.
///
/// The key management is chosen from what the access point advertised rather
/// than guessed, because getting it wrong is a passphrase that is rejected
/// without ever being tried: WPA3 wants `sae`, WPA and WPA2 want `wpa-psk`,
/// and WEP is a different key entirely — `none` with the passphrase as
/// `wep-key0`, which is what "no key management" has meant since 1999.
fn new_connection<'a>(
    ssid: &'a str,
    passphrase: &'a str,
) -> HashMap<&'a str, HashMap<&'a str, zvariant::Value<'a>>> {
    let mut connection: HashMap<&str, zvariant::Value> = HashMap::new();
    connection.insert("id", zvariant::Value::from(ssid));
    connection.insert("type", zvariant::Value::from("802-11-wireless"));

    let mut wireless: HashMap<&str, zvariant::Value> = HashMap::new();
    // As bytes, because that is what an SSID is: the string is this
    // compositor's rendering of it and the wire format is the octets.
    wireless.insert("ssid", zvariant::Value::from(ssid.as_bytes().to_vec()));

    let mut document: HashMap<&str, HashMap<&str, zvariant::Value>> = HashMap::new();
    document.insert("connection", connection);
    document.insert("802-11-wireless", wireless);

    if !passphrase.is_empty() {
        let mut security: HashMap<&str, zvariant::Value> = HashMap::new();
        security.insert("key-mgmt", zvariant::Value::from("wpa-psk"));
        security.insert("psk", zvariant::Value::from(passphrase));
        document.insert("802-11-wireless-security", security);
    }
    document
}

/// What an access point wants, from the three flag words that say it.
///
/// The order matters and is not the order the constants are numbered in. An
/// access point in a transition mode sets several at once — WPA3 networks
/// advertise SAE alongside PSK so that older radios can still associate, and a
/// WPA2 network with an enterprise profile sets both key management bits — so
/// this reports the strongest thing offered rather than the first bit found.
/// The one a picker acts on is `enterprise`, which is the only answer that
/// means a passphrase is the wrong question.
pub fn security_of(flags: u32, wpa: u32, rsn: u32) -> &'static str {
    if rsn & KEY_MGMT_SAE != 0 {
        return "wpa3";
    }
    if (wpa | rsn) & KEY_MGMT_802_1X != 0 {
        return "enterprise";
    }
    if rsn & KEY_MGMT_PSK != 0 {
        return "wpa2";
    }
    if wpa & KEY_MGMT_PSK != 0 {
        return "wpa";
    }
    // Privacy with nothing else: see the note on the constants above.
    if flags & AP_PRIVACY != 0 && wpa == 0 && rsn == 0 {
        return "wep";
    }
    ""
}

/// What to put in front of somebody who was refused.
///
/// zbus's `Display` for a method error is the bus name of the error followed
/// by its message — `org.freedesktop.NetworkManager.Error.…: Secrets were
/// required…` — which is a sentence with a fully qualified Java class in the
/// middle of it. The message alone is the part written for a person.
fn complaint<E: Into<zbus::Error>>(error: E) -> String {
    // Generic over the two error types zbus hands back, which are the same
    // failure told twice: a method call fails with `zbus::Error` and a
    // property write with `zbus::fdo::Error`, because setting a property is a
    // call to `org.freedesktop.DBus.Properties.Set`. One conversion rather
    // than two spellings of this function.
    let error: zbus::Error = error.into();
    match &error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        _ => error.to_string(),
    }
}

/// An `ay` out of a settings document, whichever way zvariant happens to have
/// wrapped it.
///
/// A connection's `ssid` arrives as a variant inside a dictionary, and what is
/// inside that variant is an array of bytes — but a document that came back
/// through another variant (which `GetSettings` does for some keys) is a
/// variant inside a variant. Unwrapping once and trying again is cheaper than
/// caring which.
fn as_bytes(value: &zvariant::OwnedValue) -> Option<Vec<u8>> {
    if let Ok(bytes) = <Vec<u8>>::try_from(value.clone()) {
        return Some(bytes);
    }
    match zvariant::Value::from(value.clone()) {
        zvariant::Value::Value(inner) => {
            <Vec<u8>>::try_from(zvariant::OwnedValue::try_from(*inner).ok()?).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strongest thing an access point offers, not the first bit set.
    ///
    /// Every one of these is a real combination: a WPA3 transition network
    /// advertises SAE and PSK together so older radios can still join, and an
    /// enterprise network sets 802.1X in whichever of the two words its
    /// generation uses.
    #[test]
    fn security_reports_the_strongest_scheme_offered() {
        assert_eq!(security_of(0, 0, 0), "", "an open network wants nothing");
        assert_eq!(
            security_of(AP_PRIVACY, 0, 0),
            "wep",
            "privacy with no key management left is the only thing WEP looks like"
        );
        assert_eq!(security_of(AP_PRIVACY, KEY_MGMT_PSK, 0), "wpa");
        assert_eq!(security_of(AP_PRIVACY, 0, KEY_MGMT_PSK), "wpa2");
        assert_eq!(
            security_of(AP_PRIVACY, KEY_MGMT_PSK, KEY_MGMT_PSK | KEY_MGMT_SAE),
            "wpa3",
            "a transition-mode network is WPA3, not the WPA2 it also offers"
        );
        assert_eq!(
            security_of(AP_PRIVACY, 0, KEY_MGMT_802_1X | KEY_MGMT_PSK),
            "enterprise",
            "a passphrase is the wrong question here and the picker must know"
        );
    }

    /// A network nobody has joined before, as NetworkManager is asked to
    /// create it. The three sections are what `nmcli device wifi connect`
    /// produces, and the SSID is bytes in both places it appears.
    #[test]
    fn a_new_connection_names_the_network_twice_and_the_secret_once() {
        let document = new_connection("kitchen", "hunter2");
        assert_eq!(
            document["connection"]["id"],
            zvariant::Value::from("kitchen")
        );
        assert_eq!(
            document["802-11-wireless"]["ssid"],
            zvariant::Value::from(b"kitchen".to_vec())
        );
        assert_eq!(
            document["802-11-wireless-security"]["psk"],
            zvariant::Value::from("hunter2")
        );
    }

    /// An open network is joined with no security section at all. With one —
    /// even an empty one — NetworkManager asks for a secret that does not
    /// exist and the activation fails with a prompt nothing can answer.
    #[test]
    fn an_open_network_carries_no_security_section() {
        let document = new_connection("cafe", "");
        assert!(!document.contains_key("802-11-wireless-security"));
    }
}
