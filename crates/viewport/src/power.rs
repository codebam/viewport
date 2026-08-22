// SPDX-License-Identifier: GPL-3.0-or-later
//
// Battery, lid and power profiles, for the bar and for the session.
//
// UPower is the one thing a laptop agrees on: a DisplayDevice that is the
// battery the bar should show, LidIsClosed for the hinge, and — on a
// different name — the power-profiles daemon for power-saver / balanced /
// performance. The compositor reads them rather than the shell, for the
// reason the shell reads nothing else either: the page has no bus.
//
// A thread of its own with a channel back, like MPRIS. UPower answers
// promptly on a healthy machine and not at all on one whose daemon has
// wedged; a compositor that waited on it would drop frames for a percentage.

use std::collections::HashMap;
use std::sync::mpsc;

use viewport_ipc::event::{PowerBattery, PowerSnapshot};

/// What closing the lid does.
///
/// Absent in the file is lock when a locker is configured, otherwise blank —
/// a laptop should do something, and a desktop has no lid so the setting
/// never fires. `"ignore"` turns it off.
///
/// The default is still read off `idle.lock_command` even though locking now
/// always means something — the shell's own screen where no command is named.
/// Deliberately conservative: a lid that locks by default on every laptop is
/// the better desktop, and it is also a machine whose PAM stack has to work
/// before the lid can be closed. Somebody who wants it writes `"lid": "lock"`,
/// which now works without a locker installed, and finds out at a keyboard
/// rather than at the bottom of a bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LidAction {
    Ignore,
    Lock,
    #[default]
    Blank,
    Suspend,
}

impl LidAction {
    /// Parse a config string. Unknown names are `None` so the caller can log
    /// them and keep whatever it had.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "ignore" => Some(Self::Ignore),
            "lock" => Some(Self::Lock),
            "blank" => Some(Self::Blank),
            "suspend" => Some(Self::Suspend),
            _ => None,
        }
    }

    /// What an absent `lid` key means. See the note on [`LidAction`] for why
    /// this still asks about the command rather than about locking.
    pub fn default_for(has_lock_command: bool) -> Self {
        if has_lock_command {
            Self::Lock
        } else {
            Self::Blank
        }
    }
}

/// What the thread sends the compositor.
#[derive(Debug)]
pub enum Message {
    Snapshot(PowerSnapshot),
}

/// The half the compositor keeps.
#[derive(Default)]
pub struct Power {
    worker: Option<mpsc::Sender<Command>>,
    enabled: bool,
    /// Whether a battery widget is on the bar. Lid policy can keep the
    /// worker alive with this off; the shell is only told when it is on.
    widget: bool,
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl Power {
    /// Where updates go. Called once, when the event loop has a source.
    pub fn attach(&mut self, events: smithay::reexports::calloop::channel::Sender<Message>) {
        self.events = Some(events);
        let enabled = self.enabled;
        self.enabled = false;
        self.set_enabled(enabled);
    }

    /// Whether the shell should be sent `power.update`.
    pub fn set_widget(&mut self, widget: bool) {
        self.widget = widget;
    }

    pub fn widget(&self) -> bool {
        self.widget
    }

    /// Whether anything wants this: a battery widget, or a lid policy.
    ///
    /// Off is the default only when both are off. A desktop with no battery
    /// widget and `"lid": "ignore"` should not be talking to UPower at all.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        let Some(events) = self.events.clone() else {
            return;
        };

        if self.worker.is_none() {
            if !enabled {
                return;
            }
            match start(events) {
                Ok(worker) => self.worker = Some(worker),
                Err(e) => {
                    tracing::warn!("power: UPower is unavailable: {e:#}");
                    return;
                }
            }
        }
        self.send(Command::Enable(enabled));
    }

    /// Switch the power profile.
    ///
    /// `&mut self` for the same reason `suspend` is: a dead worker has to be
    /// let go of before the request can be given to a new one.
    pub fn set_profile(&mut self, profile: String) {
        self.send(Command::SetProfile(profile));
    }

    /// Ask logind to suspend, reboot or power off. The worker talks to the bus;
    /// this thread does not wait.
    ///
    /// `&mut self` for one reason and only one: any of the three may land on a
    /// machine with no battery widget and a lid policy of ignore, which is the
    /// one combination that keeps the worker from ever starting — the state that
    /// still wants to be able to turn off. `ensure_worker` closes that gap
    /// without changing what `enabled` means.
    pub fn suspend(&mut self) {
        self.ensure_worker();
        self.send(Command::Suspend);
    }

    pub fn reboot(&mut self) {
        self.ensure_worker();
        self.send(Command::Reboot);
    }

    pub fn poweroff(&mut self) {
        self.ensure_worker();
        self.send(Command::Poweroff);
    }

    /// Start the worker if it is not already running, so a one-shot power action
    /// has a bus to talk logind through. Returns whether a worker exists
    /// afterwards. Deliberately does not flip `enabled`: a menu row is one call,
    /// not a request to start drawing a battery the user did not ask for.
    ///
    /// Starting is cheap here by design — a thread and a channel, with the bus
    /// connection made on the worker's own time — so this can be called from
    /// the shutdown path without anyone waiting on dbus-daemon.
    fn ensure_worker(&mut self) -> bool {
        if self.worker.is_some() {
            return true;
        }
        let Some(events) = self.events.clone() else {
            return false;
        };
        match start(events) {
            Ok(worker) => {
                self.worker = Some(worker);
                true
            }
            Err(e) => {
                tracing::warn!("power: UPower is unavailable: {e:#}");
                false
            }
        }
    }

    fn send(&mut self, command: Command) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        // A failed send means the worker thread is gone — the system bus never
        // answered, most likely. Dropping the handle here is what makes the
        // next action respawn one, rather than every suspend for the rest of
        // the session vanishing into a channel nobody reads.
        if let Err(e) = worker.send(command) {
            tracing::warn!("power: the power worker is gone ({e}); the next action restarts it");
            self.worker = None;
        }
    }
}

enum Command {
    Refresh,
    Enable(bool),
    SetProfile(String),
    Suspend,
    Reboot,
    Poweroff,
}

fn start(
    events: smithay::reexports::calloop::channel::Sender<Message>,
) -> anyhow::Result<mpsc::Sender<Command>> {
    let (commands, inbox) = mpsc::channel();
    // The thread comes up first and the bus is met inside it. `start` runs on
    // the compositor's thread, reached from the control-socket dispatch for
    // suspend, reboot and power off, and a synchronous connect to a wedged
    // dbus-daemon would freeze the desktop at the very moment somebody asked
    // it to go away. Commands queue while the connection is being made; a
    // failure is logged by the one thread that cares, and the next action
    // respawns the worker.
    std::thread::Builder::new()
        .name("power".to_owned())
        .spawn({
            let commands = commands.clone();
            move || {
                let connection = match zbus::blocking::Connection::system() {
                    Ok(connection) => connection,
                    Err(e) => {
                        tracing::warn!("power: the system bus is unavailable: {e:#}");
                        return;
                    }
                };

                // UPower's properties, the DisplayDevice's, and the profiles
                // daemon's, all through the same signal. Over-receiving is a
                // refresh; missing one is a lid that never blanks. A feed that
                // cannot be set up costs its signals only — suspend and profile
                // switches are answered without any of them.
                for (rule, what) in [
                    (
                        "type='signal',interface='org.freedesktop.DBus.Properties',\
                     sender='org.freedesktop.UPower'",
                        "UPower",
                    ),
                    (
                        "type='signal',interface='org.freedesktop.DBus.Properties',\
                     path='/org/freedesktop/UPower/PowerProfiles'",
                        "the power-profiles daemon",
                    ),
                    (
                        "type='signal',interface='org.freedesktop.DBus.Properties',\
                     path='/net/hadess/PowerProfiles'",
                        "the legacy power-profiles name",
                    ),
                ] {
                    if let Err(e) = pump(connection.clone(), commands.clone(), rule.to_owned()) {
                        tracing::warn!("power: no signal feed from {what}: {e}");
                    }
                }

                Worker::new(connection, events).run(&inbox);
            }
        })?;
    Ok(commands)
}

/// One thread reading one match rule, turning messages into commands.
///
/// The subscription is made on the reader thread rather than before spawning
/// it: registering a match rule is another blocking round trip to the bus
/// daemon, and the worker has suspend requests waiting behind it.
fn pump(
    connection: zbus::blocking::Connection,
    commands: mpsc::Sender<Command>,
    rule: String,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("power-signals".to_owned())
        .spawn(move || {
            let parsed = match zbus::MatchRule::try_from(rule.as_str()) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!("power: unreadable match rule {rule:?}: {e}");
                    return;
                }
            };
            let messages =
                match zbus::blocking::MessageIterator::for_match_rule(parsed, &connection, None) {
                    Ok(messages) => messages,
                    Err(e) => {
                        tracing::warn!("power: could not subscribe to {rule:?}: {e}");
                        return;
                    }
                };
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
    last: PowerSnapshot,
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
            last: PowerSnapshot::default(),
            enabled: false,
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Command>) {
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Enable(enabled) => {
                    self.enabled = enabled;
                    if enabled {
                        self.refresh();
                    } else {
                        self.last = PowerSnapshot::default();
                        let _ = self
                            .events
                            .send(Message::Snapshot(PowerSnapshot::default()));
                    }
                }
                // One-shot power actions are answered whether or not the periodic
                // snapshot is wanted: the setup that would otherwise keep the
                // worker off — no battery widget and a lid policy of ignore — is
                // exactly the one that still has to be able to turn the machine
                // off.
                Command::Suspend => self.suspend(),
                Command::Reboot => self.reboot(),
                Command::Poweroff => self.poweroff(),
                // A profile switch is in the same company: it arrives from the
                // bar's menu, which exists without a battery widget, and a
                // request that fell into the disabled arm below would be
                // swallowed without a word.
                Command::SetProfile(profile) => self.set_profile(&profile),
                _ if !self.enabled => {}
                Command::Refresh => self.refresh(),
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

    fn read(&self) -> PowerSnapshot {
        let manager = self.proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower",
            "org.freedesktop.UPower",
        );
        let device = self.proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/DisplayDevice",
            "org.freedesktop.UPower.Device",
        );

        let on_battery = manager
            .as_ref()
            .and_then(|p| p.get_property::<bool>("OnBattery").ok())
            .unwrap_or(false);
        let lid_closed = manager
            .as_ref()
            .and_then(|p| p.get_property::<bool>("LidIsClosed").ok())
            .unwrap_or(false);

        let mut batteries = Vec::new();
        if let Some(device) = device.as_ref() {
            let present = device.get_property::<bool>("IsPresent").unwrap_or(true);
            if present {
                let percentage = device.get_property::<f64>("Percentage").unwrap_or(-1.0);
                if percentage >= 0.0 {
                    let state = device.get_property::<u32>("State").unwrap_or(0);
                    let time_to_empty = device.get_property::<i64>("TimeToEmpty").ok();
                    let time_to_full = device.get_property::<i64>("TimeToFull").ok();
                    batteries.push(PowerBattery {
                        percentage,
                        state: battery_state(state).to_owned(),
                        time_to_empty: time_to_empty.filter(|t| *t > 0),
                        time_to_full: time_to_full.filter(|t| *t > 0),
                    });
                }
            }
        }

        let (profile, profiles) = self.profiles();
        PowerSnapshot {
            batteries,
            on_battery,
            lid_closed,
            profile,
            profiles,
        }
    }

    fn profiles(&self) -> (Option<String>, Vec<String>) {
        let proxy = self
            .proxy(
                "org.freedesktop.UPower.PowerProfiles",
                "/org/freedesktop/UPower/PowerProfiles",
                "org.freedesktop.UPower.PowerProfiles",
            )
            .or_else(|| {
                self.proxy(
                    "net.hadess.PowerProfiles",
                    "/net/hadess/PowerProfiles",
                    "net.hadess.PowerProfiles",
                )
            });
        let Some(proxy) = proxy else {
            return (None, Vec::new());
        };
        let profile = proxy.get_property::<String>("ActiveProfile").ok();
        let rows: Vec<HashMap<String, zvariant::OwnedValue>> =
            proxy.get_property("Profiles").unwrap_or_default();
        let profiles = rows
            .iter()
            .filter_map(|row| {
                row.get("Profile")
                    .and_then(|value| <&str>::try_from(value).ok())
                    .map(str::to_owned)
            })
            .collect();
        (profile, profiles)
    }

    fn set_profile(&self, profile: &str) {
        // Named rather than passed through: this is a string from a page.
        // The daemon accepts more than these three, but a bar has no business
        // inventing one — only switching to a name it was already told.
        if !self.last.profiles.iter().any(|p| p == profile)
            && !matches!(profile, "power-saver" | "balanced" | "performance")
        {
            tracing::debug!("power: no such profile {profile:?}");
            return;
        }
        let proxy = self
            .proxy(
                "org.freedesktop.UPower.PowerProfiles",
                "/org/freedesktop/UPower/PowerProfiles",
                "org.freedesktop.UPower.PowerProfiles",
            )
            .or_else(|| {
                self.proxy(
                    "net.hadess.PowerProfiles",
                    "/net/hadess/PowerProfiles",
                    "net.hadess.PowerProfiles",
                )
            });
        let Some(proxy) = proxy else {
            tracing::warn!("power: no power-profiles daemon");
            return;
        };
        if let Err(e) = proxy.set_property("ActiveProfile", profile) {
            tracing::debug!("power: setting profile {profile:?} failed: {e}");
        }
    }

    /// `Suspend`, `Reboot` or `PowerOff`. `false` is "not interactive": a lid
    /// close and a menu row are both not a prompt.
    fn logind(&self, method: &str, verb: &str) {
        let Some(proxy) = self.proxy(
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        ) else {
            tracing::warn!("power: logind is unavailable; cannot {verb}");
            return;
        };
        if let Err(e) = proxy.call::<&str, (bool,), ()>(method, &(false,)) {
            tracing::warn!("power: {verb} failed: {e}");
        }
    }

    fn suspend(&self) {
        self.logind("Suspend", "suspend");
    }

    fn reboot(&self) {
        self.logind("Reboot", "reboot");
    }

    fn poweroff(&self) {
        self.logind("PowerOff", "power off");
    }

    fn proxy(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
    ) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(destination.to_owned())
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

fn battery_state(state: u32) -> &'static str {
    match state {
        1 | 5 => "charging",
        2 | 6 => "discharging",
        3 => "empty",
        4 => "full",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lid_names() {
        assert_eq!(LidAction::parse("ignore"), Some(LidAction::Ignore));
        assert_eq!(LidAction::parse("lock"), Some(LidAction::Lock));
        assert_eq!(LidAction::parse("blank"), Some(LidAction::Blank));
        assert_eq!(LidAction::parse("suspend"), Some(LidAction::Suspend));
        assert_eq!(LidAction::parse("sleep"), None);
        assert_eq!(LidAction::parse(""), None);
    }

    #[test]
    fn absent_lid_follows_the_locker() {
        assert_eq!(LidAction::default_for(true), LidAction::Lock);
        assert_eq!(LidAction::default_for(false), LidAction::Blank);
    }

    #[test]
    fn battery_states_are_the_names_the_bar_draws() {
        assert_eq!(battery_state(1), "charging");
        assert_eq!(battery_state(2), "discharging");
        assert_eq!(battery_state(4), "full");
        assert_eq!(battery_state(0), "unknown");
    }
}
