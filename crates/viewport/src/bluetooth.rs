// SPDX-License-Identifier: GPL-3.0-or-later
//
// The Bluetooth adapter: what is near it, and pairing with one of them.
//
// BlueZ is the only Bluetooth stack on Linux and it is a bus service, so this
// is a client of it for the reason network.rs is a client of NetworkManager:
// the daemon owns the adapter, the pairing keys and the device database, and a
// compositor that talked to the kernel's sockets directly would be a second
// thing fighting over one radio.
//
// The shape of the interface is the part worth knowing before reading this.
// There is no list of devices anywhere: adapters and devices are objects under
// `/org/bluez`, and the only way to enumerate them is
// `org.freedesktop.DBus.ObjectManager.GetManagedObjects`, which returns the
// whole tree with every interface and every property in one call. That is
// convenient — one round trip rather than four per device, which is what the
// network worker pays — and it is why this file reads a nested map rather than
// building a proxy per object.
//
// A thread of its own with a channel back, like UPower and NetworkManager, and
// idle until a picker opens for the same reason: discovery is the radio
// transmitting, and one left running is a battery cost with nothing on screen
// to account for it.

use std::collections::HashMap;
use std::sync::mpsc;

use viewport_ipc::event::{BluetoothDevice, BluetoothSnapshot};

/// The bus name, and the two well-known objects under it.
const BLUEZ: &str = "org.bluez";
/// Where the tree hangs, which is the bus's root rather than `/org/bluez`:
/// `GetManagedObjects` is served at `/` and answers for everything below it.
const ROOT: &str = "/";
/// The adapter manager, which is also where the pairing agent is registered.
const AGENT_MANAGER_PATH: &str = "/org/bluez";

/// Where this compositor's pairing agent is served.
///
/// A path of its own under the project's own name, because it is an object
/// this process publishes on the system bus and BlueZ keeps it by path: two
/// programs that chose the same one could not both be registered.
const AGENT_PATH: &str = "/org/viewport/bluetooth/agent";

/// What the thread sends the compositor.
#[derive(Debug)]
pub enum Message {
    Snapshot(BluetoothSnapshot),
}

/// The half the compositor keeps. The same shape as [`crate::network::Network`]
/// and for the same reasons — see the note on that type.
#[derive(Default)]
pub struct Bluetooth {
    worker: Option<mpsc::Sender<Command>>,
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl Bluetooth {
    /// Where updates go. Called once, when the event loop has a source.
    pub fn attach(&mut self, events: smithay::reexports::calloop::channel::Sender<Message>) {
        self.events = Some(events);
    }

    /// Watch and look for devices, or stop doing both: the picker opening and
    /// closing.
    ///
    /// One call rather than two, because there is no useful state where they
    /// differ: reading the device list without discovering shows only what
    /// BlueZ already remembers, and discovering with nobody watching is a
    /// laptop quietly warming in a bag. Stopping is the half that matters, and
    /// it is why this is a message the shell sends rather than something
    /// inferred here.
    pub fn watch(&mut self, on: bool) {
        if !on && self.worker.is_none() {
            return;
        }
        self.send(Command::Watch(on));
    }

    /// Switch the adapter on or off; `None` toggles.
    pub fn power(&mut self, enabled: Option<bool>) {
        self.send(Command::Power(enabled));
    }

    /// Do one thing to one device. The verb is checked on the worker, where
    /// the list of what BlueZ will be asked lives.
    pub fn device(&mut self, address: String, action: String) {
        self.send(Command::Device { address, action });
    }

    fn send(&mut self, command: Command) {
        if self.worker.is_none() {
            let Some(events) = self.events.clone() else {
                return;
            };
            self.worker = Some(start(events));
        }
        if let Some(worker) = self.worker.as_ref() {
            let _ = worker.send(command);
        }
    }
}

enum Command {
    Refresh,
    Watch(bool),
    Power(Option<bool>),
    Device { address: String, action: String },
}

/// The agent BlueZ asks when a pairing needs a human.
///
/// Pairing does not work without one. `Device1.Pair` looks up the agent
/// belonging to the connection that called it and fails outright when there is
/// none, which is why `bluetoothctl` makes you type `agent on` before anything
/// will pair — and why an applet that skips this looks like a pairing that is
/// always refused.
///
/// Registered with the `NoInputNoOutput` capability, which is the honest
/// description of a compositor: there is no dialog here to display a six-digit
/// code in and nothing to type one on. It is also what makes the pairing
/// mode every headset, mouse and speaker uses — "just works" — go through
/// without a prompt. A device that insists on a displayed code cannot pair
/// this way, and BlueZ says so rather than hanging.
///
/// **Not the default agent.** `RequestDefaultAgent` would make this the
/// session's answer to *incoming* pairing requests as well, and auto-accepting
/// those is how a stranger on a train pairs with your laptop. Registered
/// without it, BlueZ uses this agent only for the pairings this process
/// started — which are exactly the ones somebody chose in the picker — and
/// leaves whatever else the session runs to answer for the rest.
struct PairingAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl PairingAgent {
    /// BlueZ is done with the agent. Nothing to release: this object has no
    /// state, and unregistering is the worker's business.
    fn release(&self) {}

    /// A device with no display asked to be allowed to pair. Accepted by
    /// returning: the pairing was started from the picker, which is the
    /// consent.
    fn request_authorization(&self, _device: zvariant::OwnedObjectPath) {}

    /// A paired device asked to use a profile — audio, input, a serial port.
    /// Accepted for the same reason.
    fn authorize_service(&self, _device: zvariant::OwnedObjectPath, _uuid: String) {}

    /// The pairing was abandoned from the other end.
    fn cancel(&self) {}
}

fn start(events: smithay::reexports::calloop::channel::Sender<Message>) -> mpsc::Sender<Command> {
    let (commands, inbox) = mpsc::channel();

    // Connecting happens here and not on the thread that called `start`, for
    // the reason network.rs gives: the caller is the compositor's event loop,
    // and a round trip to a wedged bus daemon must not stall a frame. A
    // machine with no bus at all is reported through the events channel, as
    // an empty snapshot with `available` false — so the picker says there is
    // nobody to ask rather than drawing an empty list that reads as "no
    // devices".
    let (worker_events, worker_commands) = (events.clone(), commands.clone());
    let spawned = std::thread::Builder::new()
        .name("bluetooth".to_owned())
        .spawn(move || {
            let connection = match zbus::blocking::Connection::system() {
                Ok(connection) => connection,
                Err(e) => {
                    tracing::warn!("bluetooth: BlueZ is unavailable: {e:#}");
                    let _ = worker_events.send(Message::Snapshot(BluetoothSnapshot::default()));
                    return;
                }
            };

            // Two rules rather than one. A property that changed — a device
            // that connected, an adapter that was powered on — arrives as
            // `PropertiesChanged`, but a device that has only just been
            // *seen* is a new object, and a new object is announced on the
            // object manager instead. A picker subscribed to only the first
            // would show the devices that were already known and never grow a
            // row for the one somebody just switched on, which is the one
            // they are waiting for.
            if let Err(e) = crate::dbus_util::pump(
                connection.clone(),
                worker_commands.clone(),
                "bluetooth-signals",
                format!(
                    "type='signal',interface='org.freedesktop.DBus.Properties',sender='{BLUEZ}'"
                ),
                |_, commands| {
                    let _ = commands.send(Command::Refresh);
                },
            ) {
                tracing::warn!("bluetooth: could not follow BlueZ's properties: {e:#}");
            }
            if let Err(e) = crate::dbus_util::pump(
                connection.clone(),
                worker_commands.clone(),
                "bluetooth-signals",
                format!(
                    "type='signal',interface='org.freedesktop.DBus.ObjectManager',sender='{BLUEZ}'"
                ),
                |_, commands| {
                    let _ = commands.send(Command::Refresh);
                },
            ) {
                tracing::warn!("bluetooth: could not follow BlueZ's objects: {e:#}");
            }

            Worker::new(connection, worker_events).run(&inbox);
        });

    if spawned.is_err() {
        tracing::warn!("bluetooth: the worker could not start");
        let _ = events.send(Message::Snapshot(BluetoothSnapshot::default()));
    }
    commands
}

struct Worker {
    connection: zbus::blocking::Connection,
    events: smithay::reexports::calloop::channel::Sender<Message>,
    last: BluetoothSnapshot,
    watching: bool,
    /// Whether this compositor started the discovery that is running. Only
    /// what it started does it stop: another program's scan is not this
    /// picker's to end, and BlueZ counts discovery per client anyway.
    discovering: bool,
    /// Whether the pairing agent is registered. Attempted once, on the first
    /// pairing rather than at startup, so a session that never opens the
    /// picker never publishes an object on the system bus.
    agent: bool,
    error: Option<String>,
}

impl Worker {
    fn new(
        connection: zbus::blocking::Connection,
        events: smithay::reexports::calloop::channel::Sender<Message>,
    ) -> Self {
        Self {
            connection,
            events,
            last: BluetoothSnapshot::default(),
            watching: false,
            discovering: false,
            agent: false,
            error: None,
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Command>) {
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Watch(on) => {
                    self.watching = on;
                    if on {
                        // The list BlueZ already has, then the radio: a picker
                        // that showed nothing until discovery found something
                        // would hide the headset that is already paired.
                        self.refresh();
                        self.error = self.set_discovery(Some(true)).err();
                        self.refresh();
                    } else {
                        // The radio first, then the bookkeeping: a picker that
                        // closed must not leave a scan running.
                        // Ignored deliberately: the picker has gone and there
                        // is nowhere left to report a stop that failed.
                        let _ = self.set_discovery(Some(false));
                        self.last = BluetoothSnapshot::default();
                        self.error = None;
                    }
                }
                _ if !self.watching => {}
                Command::Refresh => self.refresh(),
                Command::Power(enabled) => {
                    self.error = self.set_power(enabled).err();
                    self.refresh();
                }
                Command::Device { address, action } => {
                    self.error = self.act(&address, &action).err();
                    if let Some(e) = self.error.as_ref() {
                        tracing::info!("bluetooth: {action} {address} failed: {e}");
                    }
                    self.refresh();
                }
            }
        }
    }

    fn refresh(&mut self) {
        let snapshot = self.read();
        if snapshot == self.last {
            return;
        }
        self.last = snapshot.clone();
        let _ = self.events.send(Message::Snapshot(snapshot));
    }

    /// The whole tree in one call, turned into the two things a picker draws.
    fn read(&self) -> BluetoothSnapshot {
        let Some(objects) = self.objects() else {
            return BluetoothSnapshot::default();
        };

        let mut snapshot = BluetoothSnapshot {
            error: self.error.clone(),
            ..BluetoothSnapshot::default()
        };

        // The first adapter, which on every machine that has one is the only
        // machine that has two — a dongle beside a built-in radio — and a
        // picker offering both would be asking a question about hardware
        // rather than about the headset somebody wants to use.
        for (path, interfaces) in &objects {
            let Some(adapter) = interfaces.get("org.bluez.Adapter1") else {
                continue;
            };
            snapshot.available = true;
            snapshot.powered = flag(adapter, "Powered");
            snapshot.discovering = flag(adapter, "Discovering");
            snapshot.adapter = text(adapter, "Alias");
            let _ = path;
            break;
        }

        if !snapshot.available {
            return snapshot;
        }

        for interfaces in objects.values() {
            let Some(device) = interfaces.get("org.bluez.Device1") else {
                continue;
            };
            let address = text(device, "Address");
            if address.is_empty() {
                continue;
            }
            snapshot.devices.push(BluetoothDevice {
                name: text(device, "Alias"),
                icon: text(device, "Icon"),
                paired: flag(device, "Paired"),
                trusted: flag(device, "Trusted"),
                connected: flag(device, "Connected"),
                // Absent for a device BlueZ remembers but cannot hear right
                // now, which is how a picker tells "my headset, in a drawer"
                // from "my headset, switched on beside me".
                rssi: device.get("RSSI").and_then(as_i16),
                address,
            });
        }

        // What is in use, then what is known, then whatever the scan turned
        // up — and alphabetically inside each group, so that a list refreshing
        // twice a second under a moving signal does not reorder itself while
        // somebody is reaching for a row.
        snapshot.devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(a.name.cmp(&b.name))
                .then(a.address.cmp(&b.address))
        });
        snapshot
    }

    /// `GetManagedObjects`, which is the only way to enumerate BlueZ.
    fn objects(&self) -> Option<Objects> {
        let proxy = self.proxy(ROOT, "org.freedesktop.DBus.ObjectManager")?;
        proxy
            .call::<&str, (), Objects>("GetManagedObjects", &())
            .ok()
    }

    /// The first adapter's object path, which every adapter call needs.
    fn adapter(&self) -> Option<zvariant::OwnedObjectPath> {
        self.objects()?
            .into_iter()
            .find(|(_, interfaces)| interfaces.contains_key("org.bluez.Adapter1"))
            .map(|(path, _)| path)
    }

    /// The object path of the device with this address.
    ///
    /// By address rather than by path because a path is BlueZ's spelling of an
    /// address — `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` — and reconstructing
    /// it would be this compositor guessing at another program's naming
    /// scheme, including which adapter the device was found on.
    fn device_path(&self, address: &str) -> Option<zvariant::OwnedObjectPath> {
        self.objects()?.into_iter().find_map(|(path, interfaces)| {
            let device = interfaces.get("org.bluez.Device1")?;
            (text(device, "Address").eq_ignore_ascii_case(address)).then_some(path)
        })
    }

    /// Start or stop looking for devices; `None` toggles what is running now.
    fn set_discovery(&mut self, enabled: Option<bool>) -> Result<(), String> {
        let wanted = enabled.unwrap_or(!self.discovering);
        if wanted == self.discovering {
            return Ok(());
        }
        // Nothing to stop, and nothing this compositor started: leave the
        // radio alone rather than calling StopDiscovery into an error.
        let Some(adapter) = self.adapter() else {
            return if wanted {
                Err("there is no Bluetooth adapter".to_owned())
            } else {
                Ok(())
            };
        };
        let proxy = self
            .proxy(adapter.as_str(), "org.bluez.Adapter1")
            .ok_or_else(|| "BlueZ is not answering".to_owned())?;
        let method = if wanted {
            "StartDiscovery"
        } else {
            "StopDiscovery"
        };
        proxy.call::<&str, (), ()>(method, &()).map_err(complaint)?;
        self.discovering = wanted;
        Ok(())
    }

    /// Switch the adapter on or off; `None` toggles.
    fn set_power(&mut self, enabled: Option<bool>) -> Result<(), String> {
        let adapter = self
            .adapter()
            .ok_or_else(|| "there is no Bluetooth adapter".to_owned())?;
        let proxy = self
            .proxy(adapter.as_str(), "org.bluez.Adapter1")
            .ok_or_else(|| "BlueZ is not answering".to_owned())?;
        // Read rather than remembered: `rfkill` and another applet both change
        // this without anything here hearing about it first.
        let enabled = enabled.unwrap_or(!proxy.get_property::<bool>("Powered").unwrap_or(false));
        proxy.set_property("Powered", enabled).map_err(complaint)?;
        if !enabled {
            // The adapter going down takes any discovery with it, and BlueZ
            // does not say so on a signal this would notice.
            self.discovering = false;
        }
        Ok(())
    }

    /// One verb, against one device.
    ///
    /// `connect` is the compound one, because it is the only one a picker
    /// sends for a row somebody tapped: an unpaired device is paired first,
    /// then trusted so that BlueZ brings it back on its own when it is
    /// switched on again, and then connected. Doing that here rather than
    /// making the shell send three messages keeps the order in one place —
    /// connecting before pairing fails, and trusting after connecting leaves a
    /// headset that has to be paired again tomorrow.
    fn act(&mut self, address: &str, action: &str) -> Result<(), String> {
        // Named rather than passed through: this is a string from a page, and
        // `org.bluez.Device1` has methods a picker has no business calling.
        if !matches!(
            action,
            "pair" | "connect" | "disconnect" | "trust" | "untrust" | "forget"
        ) {
            return Err(format!("no such action {action:?}"));
        }

        let path = self
            .device_path(address)
            .ok_or_else(|| "that device is not there any more".to_owned())?;
        let device = self
            .proxy(path.as_str(), "org.bluez.Device1")
            .ok_or_else(|| "BlueZ is not answering".to_owned())?;

        match action {
            "trust" | "untrust" => {
                return device
                    .set_property("Trusted", action == "trust")
                    .map_err(complaint);
            }
            "forget" => {
                let adapter = self
                    .adapter()
                    .ok_or_else(|| "there is no Bluetooth adapter".to_owned())?;
                let proxy = self
                    .proxy(adapter.as_str(), "org.bluez.Adapter1")
                    .ok_or_else(|| "BlueZ is not answering".to_owned())?;
                return proxy
                    .call::<&str, _, ()>("RemoveDevice", &(path.as_ref(),))
                    .map_err(complaint);
            }
            "disconnect" => {
                return device
                    .call::<&str, (), ()>("Disconnect", &())
                    .map_err(complaint);
            }
            _ => {}
        }

        let paired = device.get_property::<bool>("Paired").unwrap_or(false);
        if !paired {
            self.register_agent();
            device
                .call::<&str, (), ()>("Pair", &())
                .map_err(complaint)?;
            // Trusted straight after pairing, and only then: this is the bit
            // that lets a headset reconnect by itself, and a device that was
            // never paired has nothing to be trusted for.
            if let Err(e) = device.set_property("Trusted", true) {
                tracing::debug!("bluetooth: trusting {address} failed: {e}");
            }
        }
        if action == "pair" {
            return Ok(());
        }
        device
            .call::<&str, (), ()>("Connect", &())
            .map_err(complaint)
    }

    /// Publish the pairing agent and tell BlueZ about it, once.
    ///
    /// Failure is logged and not returned: the pairing that follows either
    /// works — some devices need no agent at all — or fails with BlueZ's own
    /// complaint, which is a better thing to put in front of somebody than a
    /// message about agent registration.
    fn register_agent(&mut self) {
        if self.agent {
            return;
        }
        // Marked done either way. A registration that failed once fails the
        // same way every time, and retrying it on each pairing would be a bus
        // round trip spent to log the same line again.
        self.agent = true;

        if let Err(e) = self.connection.object_server().at(AGENT_PATH, PairingAgent) {
            tracing::warn!("bluetooth: the pairing agent could not be published: {e}");
            return;
        }
        let Some(manager) = self.proxy(AGENT_MANAGER_PATH, "org.bluez.AgentManager1") else {
            return;
        };
        let path = match zvariant::ObjectPath::try_from(AGENT_PATH) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!("bluetooth: {AGENT_PATH} is not an object path: {e}");
                return;
            }
        };
        // See the note on `PairingAgent` for why this capability, and why
        // `RequestDefaultAgent` is not called after it.
        if let Err(e) = manager.call::<&str, _, ()>("RegisterAgent", &(path, "NoInputNoOutput")) {
            tracing::warn!("bluetooth: BlueZ refused the pairing agent: {e}");
        }
    }

    fn proxy(&self, path: &str, interface: &str) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(BLUEZ.to_owned())
            .ok()?
            .path(path.to_owned())
            .ok()?
            .interface(interface.to_owned())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    }
}

/// What `GetManagedObjects` answers with: every object, every interface on it,
/// and every property of each.
type Objects = HashMap<zvariant::OwnedObjectPath, Interfaces>;
type Interfaces = HashMap<String, HashMap<String, zvariant::OwnedValue>>;

/// A boolean property, false when it is absent or of another type.
///
/// Absent is the common case rather than the error case: BlueZ omits a
/// property it has nothing to say about, so a device that has never been seen
/// has no `Connected` at all.
fn flag(properties: &HashMap<String, zvariant::OwnedValue>, name: &str) -> bool {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

/// A string property, empty when it is absent.
fn text(properties: &HashMap<String, zvariant::OwnedValue>, name: &str) -> String {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .unwrap_or_default()
        .to_owned()
}

/// `RSSI`, which is signed 16-bit and only present while a device is in range.
fn as_i16(value: &zvariant::OwnedValue) -> Option<i16> {
    i16::try_from(value.clone()).ok()
}

/// The half of a bus error that was written for a person.
///
/// See [`crate::dbus_util::complaint`], which is this and which the tray and
/// the network picker share.
fn complaint<E: Into<zbus::Error>>(error: E) -> String {
    crate::dbus_util::complaint(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order a picker draws devices in, which is not the order BlueZ
    /// hands them over: `GetManagedObjects` answers in whatever order the
    /// objects happen to sit in.
    ///
    /// Connected first, then paired, then alphabetically — and alphabetically
    /// inside each group rather than by signal strength, which is the one
    /// property that moves while somebody is reaching for a row.
    #[test]
    fn devices_are_drawn_in_use_then_known_then_found() {
        let device = |name: &str, paired, connected| BluetoothDevice {
            address: format!("00:00:00:00:00:{:02}", name.len()),
            name: name.to_owned(),
            icon: String::new(),
            paired,
            trusted: false,
            connected,
            rssi: None,
        };
        let mut devices = [
            device("speaker", false, false),
            device("headset", true, false),
            device("aaa", false, false),
            device("mouse", true, true),
        ];
        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(a.name.cmp(&b.name))
                .then(a.address.cmp(&b.address))
        });
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["mouse", "headset", "aaa", "speaker"]);
    }

    /// A property BlueZ did not send is not an error and not a panic. Every
    /// device answers for a different subset of `Device1` — one that has never
    /// been connected has no `Connected` property at all — so every read here
    /// has to survive its absence.
    #[test]
    fn absent_properties_read_as_nothing() {
        let properties: HashMap<String, zvariant::OwnedValue> = HashMap::new();
        assert!(!flag(&properties, "Paired"));
        assert_eq!(text(&properties, "Alias"), "");
        assert_eq!(properties.get("RSSI").and_then(as_i16), None);
    }
}
