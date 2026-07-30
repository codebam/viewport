// SPDX-License-Identifier: GPL-3.0-or-later
//
// Dark mode, via the desktop settings portal. Ports src/appearance.c.
//
// A compositor cannot make its clients dark by styling anything: Firefox, GTK
// and Qt apps each decide for themselves, and they all ask the same question —
// the `color-scheme` key in the `org.freedesktop.appearance` namespace, read
// over D-Bus through xdg-desktop-portal. With nothing answering, every
// application falls back to light however the shell looks.
//
// Normally that answer comes from a desktop environment. There is not one here,
// so the compositor answers directly by implementing
// org.freedesktop.impl.portal.Settings. Deliberately not by setting GSettings
// keys: that route needs dconf and GNOME's schemas installed, and silently does
// nothing when they are absent.
//
// It shares the D-Bus thread arrangement with `notification.rs` — zbus wants an
// async runtime and this loop is GLib with calloop inside it — but not its
// channel: everything here is answered from state the object already holds, so
// nothing has to reach the compositor to reply.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zvariant::{OwnedValue, Value};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.viewport";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";
const GNOME: &str = "org.gnome.desktop.interface";

/// `color-scheme` as the specification numbers it.
///
/// The numbers are the interface: a client reads the integer, so 1 has to mean
/// dark whatever this compositor would rather call it.
pub const PREFER_DARK: u32 = 1;
pub const PREFER_LIGHT: u32 = 2;

/// What the portal answers with.
#[derive(Debug, Clone)]
pub struct Settings {
    pub color_scheme: u32,
    /// The cursor the compositor actually draws. A toolkit sizes its own
    /// cursors from these, and a disagreement here is a cursor that changes
    /// size as it crosses from a window into the desktop.
    pub cursor_theme: String,
    pub cursor_size: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            color_scheme: PREFER_DARK,
            cursor_theme: "default".to_owned(),
            cursor_size: 24,
        }
    }
}

/// A value the way D-Bus carries it.
///
/// Infallible for everything here: only file descriptors can fail to be owned,
/// and a setting is never one.
fn owned<'a, T: Into<Value<'a>>>(value: T) -> OwnedValue {
    value
        .into()
        .try_to_owned()
        .expect("a setting is never a file descriptor")
}

/// The `org.gnome.desktop.interface` keys.
///
/// Answering only color-scheme is worse than providing no portal at all: a
/// client that finds a working Settings implementation trusts it instead of
/// falling back to its own defaults, so a missing font or text-scaling value
/// becomes a measurement of zero rather than a sensible default. Toolkits size
/// menus from exactly these keys.
///
/// The values are conventional defaults rather than anything clever — the point
/// is that every key a toolkit asks for gets a usable answer.
pub fn gnome_settings(settings: &Settings) -> HashMap<String, OwnedValue> {
    let dark = settings.color_scheme == PREFER_DARK;
    let mut keys = HashMap::new();
    keys.insert(
        "color-scheme".to_owned(),
        owned(if dark { "prefer-dark" } else { "prefer-light" }),
    );
    keys.insert(
        "gtk-theme".to_owned(),
        owned(if dark { "Adwaita-dark" } else { "Adwaita" }),
    );
    keys.insert("icon-theme".to_owned(), owned("Adwaita"));
    keys.insert("font-name".to_owned(), owned("Sans 10"));
    keys.insert("monospace-font-name".to_owned(), owned("Monospace 10"));
    keys.insert("document-font-name".to_owned(), owned("Sans 10"));
    keys.insert("text-scaling-factor".to_owned(), owned(1.0f64));
    keys.insert(
        "cursor-theme".to_owned(),
        owned(settings.cursor_theme.as_str()),
    );
    keys.insert("cursor-size".to_owned(), owned(settings.cursor_size));
    keys.insert("enable-animations".to_owned(), owned(true));
    keys
}

/// The `org.freedesktop.appearance` keys.
pub fn appearance_settings(settings: &Settings) -> HashMap<String, OwnedValue> {
    let mut keys = HashMap::new();
    keys.insert("color-scheme".to_owned(), owned(settings.color_scheme));
    // Documented in the same namespace, and clients read both.
    keys.insert("contrast".to_owned(), owned(0u32));
    keys
}

/// One setting, or `None` when the key is genuinely unknown.
///
/// Unknown has to stay distinguishable from absent: the specification requires
/// an error for a key that does not exist, and clients fall back cleanly on it
/// where they would take a zero at face value.
pub fn lookup(settings: &Settings, namespace: &str, key: &str) -> Option<OwnedValue> {
    match namespace {
        APPEARANCE => appearance_settings(settings).remove(key),
        GNOME => gnome_settings(settings).remove(key),
        _ => None,
    }
}

/// Every namespace, as `ReadAll` returns them.
///
/// The requested patterns are ignored, as in `src/appearance.c:151`: a client
/// asking for one namespace and being given two has what it asked for, and
/// matching the specification's glob syntax here would be a parser whose only
/// effect is to withhold answers.
pub fn read_all(settings: &Settings) -> HashMap<String, HashMap<String, OwnedValue>> {
    HashMap::from([
        (APPEARANCE.to_owned(), appearance_settings(settings)),
        (GNOME.to_owned(), gnome_settings(settings)),
    ])
}

/// The half the compositor keeps.
#[derive(Default)]
pub struct Appearance {
    settings: Arc<Mutex<Settings>>,
    /// Kept to emit `SettingChanged`, and `None` when the name could not be
    /// claimed.
    connection: Option<zbus::blocking::Connection>,
}

impl Appearance {
    /// Claim the name and start answering.
    ///
    /// A failure is not fatal: a session with no D-Bus, or one where a real
    /// desktop portal already owns the name, still has a working compositor —
    /// its applications simply keep their own defaults, which is what they had
    /// a moment ago.
    pub fn start(
        &mut self,
        settings: Settings,
        screencast: crate::screencast::portal::ScreenCast,
    ) -> anyhow::Result<()> {
        let scheme = settings.color_scheme;
        *self.settings.lock().unwrap() = settings;

        let portal = Portal {
            settings: self.settings.clone(),
        };
        // One connection for both interfaces, because they share a bus name.
        //
        // A second connection claiming org.freedesktop.impl.portal.desktop.
        // viewport does not get it — the first one holds it — so whichever
        // interface was built second was simply absent from the bus, and the
        // portal frontend fell through to another backend without saying
        // anything. Settings and ScreenCast live at the same object path, as
        // they do in every other portal implementation.
        //
        // The name is claimed if it is free and left alone if it is not,
        // rather than replacing whoever holds it: a real desktop portal
        // running alongside knows more about the session than this does.
        // Before it is handed over: the watcher needs the same sessions the
        // object on the bus is keeping.
        let sessions = screencast.sessions();
        let closer = screencast.closer();

        let connection = zbus::blocking::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, portal)?
            .serve_at(OBJECT_PATH, screencast)?
            .build()?;

        crate::screencast::portal::watch_frontend(connection.clone(), sessions, closer);

        self.connection = Some(connection);
        tracing::info!("settings and screencast portals up, color-scheme={scheme}");
        Ok(())
    }

    pub fn is_dark(&self) -> bool {
        self.settings.lock().unwrap().color_scheme == PREFER_DARK
    }

    /// Switch the session between dark and light.
    ///
    /// Running applications change on the signal rather than at next start,
    /// which is the whole reason the portal is a service and not a file.
    pub fn set_dark(&mut self, dark: bool) {
        let value = if dark { PREFER_DARK } else { PREFER_LIGHT };
        {
            let mut settings = self.settings.lock().unwrap();
            if settings.color_scheme == value {
                return;
            }
            settings.color_scheme = value;
        }

        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        if let Err(e) = connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            "SettingChanged",
            &(APPEARANCE, "color-scheme", Value::from(value)),
        ) {
            tracing::warn!("could not announce the colour scheme: {e}");
        }
        tracing::info!("color-scheme now {}", if dark { "dark" } else { "light" });
    }
}

/// The object on the bus.
struct Portal {
    settings: Arc<Mutex<Settings>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl Portal {
    fn read_all(&self, _namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        read_all(&self.settings.lock().unwrap())
    }

    fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        lookup(&self.settings.lock().unwrap(), namespace, key).ok_or_else(|| {
            // This specific error, because it is what a client checks for
            // before falling back to its own default.
            zbus::fdo::Error::UnknownProperty(format!("unknown setting {namespace}/{key}"))
        })
    }

    /// Announced when the scheme changes, so a running application switches
    /// rather than waiting to be restarted.
    ///
    /// Declared here only to put it in the introspection — it is emitted
    /// through the connection, from `set_dark`, where the compositor knows the
    /// scheme changed. A client subscribes by match rule either way.
    #[zbus(signal)]
    async fn setting_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Value<'_>,
    ) -> zbus::Result<()>;

    // Lower case, and named explicitly because zbus would otherwise publish it
    // as `Version` — the interface spells it `version`, and a client reading
    // the version to decide what it may ask for finds nothing at all.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> Settings {
        Settings {
            color_scheme: PREFER_DARK,
            cursor_theme: "Bibata".to_owned(),
            cursor_size: 32,
        }
    }

    #[test]
    fn the_scheme_is_the_number_clients_read() {
        // The integer is the interface. Naming them the other way round makes
        // every application light while the shell is dark, and nothing in the
        // log says so.
        let keys = appearance_settings(&dark());
        assert_eq!(u32::try_from(&keys["color-scheme"]).unwrap(), 1);
        let light = Settings {
            color_scheme: PREFER_LIGHT,
            ..dark()
        };
        assert_eq!(
            u32::try_from(&appearance_settings(&light)["color-scheme"]).unwrap(),
            2
        );
    }

    #[test]
    fn the_gnome_namespace_says_the_same_thing_in_its_own_words() {
        // GTK reads the string here rather than the integer above, so the two
        // have to agree — a client that got them from different sources would
        // draw a dark window with a light titlebar.
        let keys = gnome_settings(&dark());
        assert_eq!(
            String::try_from(keys["color-scheme"].clone()).unwrap(),
            "prefer-dark"
        );
        assert_eq!(
            String::try_from(keys["gtk-theme"].clone()).unwrap(),
            "Adwaita-dark"
        );
    }

    #[test]
    fn every_key_a_toolkit_asks_for_has_an_answer() {
        // A client that finds a working portal trusts it instead of falling
        // back, so a missing font becomes a measurement of zero rather than a
        // sensible default.
        let keys = gnome_settings(&dark());
        for key in [
            "font-name",
            "monospace-font-name",
            "document-font-name",
            "text-scaling-factor",
            "icon-theme",
            "cursor-theme",
            "cursor-size",
            "enable-animations",
        ] {
            assert!(keys.contains_key(key), "{key} has no answer");
        }
    }

    #[test]
    fn the_cursor_reported_is_the_cursor_drawn() {
        // A toolkit sizes its own cursors from these. Reporting a default while
        // the compositor draws something else is a pointer that changes size as
        // it crosses from a window onto the desktop.
        let keys = gnome_settings(&dark());
        assert_eq!(
            String::try_from(keys["cursor-theme"].clone()).unwrap(),
            "Bibata"
        );
        assert_eq!(i32::try_from(&keys["cursor-size"]).unwrap(), 32);
    }

    #[test]
    fn an_unknown_key_is_unknown_rather_than_zero() {
        // The specification requires an error, and clients fall back cleanly on
        // it where they would take a zero at face value.
        assert!(lookup(&dark(), APPEARANCE, "colour-scheme").is_none());
        assert!(lookup(&dark(), "com.example.whatever", "color-scheme").is_none());
        assert!(lookup(&dark(), APPEARANCE, "color-scheme").is_some());
        assert!(lookup(&dark(), GNOME, "font-name").is_some());
    }

    #[test]
    fn read_all_carries_both_namespaces() {
        let all = read_all(&dark());
        assert!(all.contains_key(APPEARANCE));
        assert!(all.contains_key(GNOME));
    }
}
