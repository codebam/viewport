// SPDX-License-Identifier: GPL-3.0-or-later
//
// org.freedesktop.impl.portal.InputCapture and its receiver-side EI transport.

use std::{
    collections::{HashMap, HashSet},
    os::fd::OwnedFd,
    sync::{Arc, Mutex},
};

use enumflags2::BitFlags;
use reis::{
    calloop::{EisRequestSource, EisRequestSourceEvent},
    eis,
    request::{Device, DeviceCapability, EisRequest},
};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::keyboard::{FilterResult, KeymapFile, ModifiersState, SerializedMods},
    reexports::calloop::{PostAction, RegistrationToken},
    utils::{Logical, Point, SERIAL_COUNTER},
};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::screencast::portal::{Message, SessionObject, Sessions};

pub const KEYBOARD: u32 = 1;
pub const POINTER: u32 = 2;
pub const SUPPORTED_CAPABILITIES: u32 = KEYBOARD | POINTER;

const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.InputCapture";
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Barrier {
    pub id: u32,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

#[derive(Debug)]
struct Session {
    generation: u64,
    app_id: String,
    started: bool,
    capabilities: u32,
    enabled: bool,
    eis: bool,
    barriers: Vec<Barrier>,
}

#[derive(Debug)]
struct State {
    sessions: HashMap<OwnedObjectPath, Session>,
    zones: Vec<Zone>,
    zone_set: u32,
    next_generation: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            zones: Vec::new(),
            zone_set: 1,
            next_generation: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct Shared(Arc<Mutex<State>>);

impl Shared {
    pub fn update_zones(&self, zones: Vec<Zone>) -> Vec<OwnedObjectPath> {
        let mut state = self.0.lock().unwrap();
        if state.zones == zones {
            return Vec::new();
        }
        state.zones = zones;
        state.zone_set = state.zone_set.wrapping_add(1).max(1);
        for session in state.sessions.values_mut() {
            session.barriers.clear();
        }
        state.sessions.keys().cloned().collect()
    }

    fn remove(&self, path: &OwnedObjectPath, generation: u64) {
        let mut state = self.0.lock().unwrap();
        if state
            .sessions
            .get(path)
            .is_some_and(|session| session_generation_matches(session, generation))
        {
            state.sessions.remove(path);
        }
    }
}

#[derive(Clone, Default)]
pub struct Signals(Arc<Mutex<Option<zbus::blocking::Connection>>>);

impl Signals {
    pub fn set_connection(&self, connection: zbus::blocking::Connection) {
        *self.0.lock().unwrap() = Some(connection);
    }

    fn emit(&self, member: &str, session: &OwnedObjectPath, options: HashMap<String, OwnedValue>) {
        let connection = self.0.lock().unwrap().clone();
        let Some(connection) = connection else { return };
        if let Err(error) = connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            member,
            &(session.as_ref(), options),
        ) {
            tracing::warn!("input capture: could not emit {member}: {error}");
        }
    }

    pub fn disabled(&self, session: &OwnedObjectPath) {
        self.emit("Disabled", session, HashMap::new());
    }

    pub fn zones_changed(&self, session: &OwnedObjectPath) {
        self.emit("ZonesChanged", session, HashMap::new());
    }

    fn activated(
        &self,
        session: &OwnedObjectPath,
        activation_id: u32,
        cursor: Point<f64, Logical>,
        barrier_id: u32,
    ) {
        let mut options = HashMap::new();
        options.insert("activation_id".into(), OwnedValue::from(activation_id));
        options.insert(
            "cursor_position".into(),
            OwnedValue::try_from(zvariant::Value::from((cursor.x, cursor.y))).expect("tuple"),
        );
        options.insert("barrier_id".into(), OwnedValue::from(barrier_id));
        self.emit("Activated", session, options);
    }

    fn deactivated(
        &self,
        session: &OwnedObjectPath,
        activation_id: u32,
        cursor: Point<f64, Logical>,
    ) {
        let mut options = HashMap::new();
        options.insert("activation_id".into(), OwnedValue::from(activation_id));
        options.insert(
            "cursor_position".into(),
            OwnedValue::try_from(zvariant::Value::from((cursor.x, cursor.y))).expect("tuple"),
        );
        self.emit("Deactivated", session, options);
    }
}

pub struct InputCapture {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Sessions,
    shared: Shared,
}

impl InputCapture {
    pub fn new(
        sender: smithay::reexports::calloop::channel::Sender<Message>,
        sessions: Sessions,
        shared: Shared,
    ) -> Self {
        Self {
            sender,
            sessions,
            shared,
        }
    }

    fn authenticated(&self, header: &zbus::message::Header<'_>) -> bool {
        crate::screencast::portal::called_by_frontend(&self.sessions, "input capture", header)
    }

    fn valid_session(&self, path: &OwnedObjectPath, app_id: &str) -> Result<(), zbus::fdo::Error> {
        let state = self.shared.0.lock().unwrap();
        match state.sessions.get(path) {
            Some(session) if session.app_id == app_id => Ok(()),
            Some(_) => Err(zbus::fdo::Error::AccessDenied(
                "session belongs to another application".into(),
            )),
            None => Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown session {path}"
            ))),
        }
    }

    async fn create(
        &self,
        path: OwnedObjectPath,
        app_id: &str,
        server: &zbus::ObjectServer,
        owner: Option<String>,
    ) -> Result<(), String> {
        let generation = {
            let mut state = self.shared.0.lock().unwrap();
            state.next_generation = state.next_generation.wrapping_add(1).max(1);
            state.next_generation
        };
        let displaced = self.sessions.lock().unwrap().sessions.insert(
            path.clone(),
            crate::screencast::portal::Session::new_input_capture(app_id, owner, generation),
        );
        if let Some(displaced) = displaced {
            crate::screencast::portal::release_session(&displaced, &path, &self.sender);
            let _ = server.remove::<SessionObject, _>(&path).await;
        }
        self.shared.0.lock().unwrap().sessions.insert(
            path.clone(),
            Session {
                generation,
                app_id: app_id.to_owned(),
                started: false,
                capabilities: 0,
                enabled: false,
                eis: false,
                barriers: Vec::new(),
            },
        );
        if let Err(error) = server
            .at(
                &path,
                SessionObject {
                    path: path.clone(),
                    sender: self.sender.clone(),
                    sessions: self.sessions.clone(),
                },
            )
            .await
        {
            self.shared.remove(&path, generation);
            self.sessions.lock().unwrap().sessions.remove(&path);
            return Err(error.to_string());
        }
        Ok(())
    }

    async fn consent(&self, app_id: &str, capabilities: u32) -> Result<u32, String> {
        let (reply, answer) = async_channel::bounded(1);
        self.sender
            .send(Message::StartInputCapture {
                app_id: app_id.to_owned(),
                capabilities,
                reply,
            })
            .map_err(|_| "the compositor is not listening".to_owned())?;
        answer
            .recv()
            .await
            .map_err(|_| "the compositor did not answer".to_owned())?
    }

    async fn start_session(
        &self,
        path: &OwnedObjectPath,
        app_id: &str,
        capabilities: u32,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let capabilities = capabilities & SUPPORTED_CAPABILITIES;
        if capabilities == 0 {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let generation = {
            let state = self.shared.0.lock().unwrap();
            let Some(session) = state.sessions.get(path) else {
                return (RESPONSE_FAILED, HashMap::new());
            };
            if session.app_id != app_id || session.started {
                return (RESPONSE_FAILED, HashMap::new());
            }
            session.generation
        };
        let granted = match self.consent(app_id, capabilities).await {
            Ok(granted) => granted & capabilities,
            Err(error) => {
                tracing::warn!("input capture: {error}");
                return (RESPONSE_CANCELLED, HashMap::new());
            }
        };
        let mut state = self.shared.0.lock().unwrap();
        let Some(session) = state.sessions.get_mut(path) else {
            return (RESPONSE_CANCELLED, HashMap::new());
        };
        if session.app_id != app_id || session.started || session.generation != generation {
            return (RESPONSE_FAILED, HashMap::new());
        }
        session.started = true;
        session.capabilities = granted;
        let mut results = HashMap::new();
        results.insert("capabilities".into(), OwnedValue::from(granted));
        results.insert("clipboard_enabled".into(), OwnedValue::from(false));
        (RESPONSE_SUCCESS, results)
    }

    fn set_enabled(
        &self,
        path: OwnedObjectPath,
        app_id: &str,
        enabled: bool,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let mut state = self.shared.0.lock().unwrap();
        let Some(session) = state.sessions.get_mut(&path) else {
            return (RESPONSE_FAILED, HashMap::new());
        };
        if session.app_id != app_id || !session.started || (enabled && !session.eis) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        session.enabled = enabled;
        let generation = session.generation;
        drop(state);
        if !enabled {
            let _ = self.sender.send(Message::DisableInputCapture {
                session: path,
                generation,
            });
        }
        (RESPONSE_SUCCESS, HashMap::new())
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.InputCapture")]
impl InputCapture {
    #[zbus(property, name = "SupportedCapabilities")]
    fn supported_capabilities(&self) -> u32 {
        SUPPORTED_CAPABILITIES
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    #[allow(clippy::too_many_arguments)] // Fixed by the portal D-Bus interface.
    async fn create_session(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let capabilities = options
            .get("capabilities")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let path = OwnedObjectPath::from(session_handle);
        if self
            .create(
                path.clone(),
                app_id,
                server,
                header.sender().map(ToString::to_string),
            )
            .await
            .is_err()
        {
            return (RESPONSE_FAILED, HashMap::new());
        }
        self.start_session(&path, app_id, capabilities).await
    }

    async fn create_session2(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        if !self.authenticated(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "not the portal frontend".into(),
            ));
        }
        let path = OwnedObjectPath::from(session_handle);
        self.create(
            path,
            app_id,
            server,
            header.sender().map(ToString::to_string),
        )
        .await
        .map_err(zbus::fdo::Error::Failed)?;
        Ok(HashMap::new())
    }

    async fn start(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let capabilities = options
            .get("capabilities")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        self.start_session(&OwnedObjectPath::from(session_handle), app_id, capabilities)
            .await
    }

    fn get_zones(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let path = OwnedObjectPath::from(session_handle);
        if self.valid_session(&path, app_id).is_err() {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let state = self.shared.0.lock().unwrap();
        let zones: Vec<(u32, u32, i32, i32)> = state
            .zones
            .iter()
            .map(|z| (z.width, z.height, z.x, z.y))
            .collect();
        let mut results = HashMap::new();
        results.insert(
            "zones".into(),
            OwnedValue::try_from(zvariant::Value::from(zones)).expect("zones"),
        );
        results.insert("zone_set".into(), OwnedValue::from(state.zone_set));
        (RESPONSE_SUCCESS, results)
    }

    #[allow(clippy::too_many_arguments)] // Fixed by the portal D-Bus interface.
    fn set_pointer_barriers(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let path = OwnedObjectPath::from(session_handle);
        let mut state = self.shared.0.lock().unwrap();
        if state.zone_set != zone_set {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let zones = state.zones.clone();
        let Some(session) = state.sessions.get_mut(&path) else {
            return (RESPONSE_FAILED, HashMap::new());
        };
        if session.app_id != app_id || !session.started {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let mut accepted = Vec::new();
        let mut failed = Vec::new();
        let mut ids = HashSet::new();
        for barrier in barriers {
            let id = barrier
                .get("barrier_id")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            let position = barrier
                .get("position")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| <(i32, i32, i32, i32)>::try_from(value).ok());
            let candidate = position.map(|(x1, y1, x2, y2)| Barrier { id, x1, y1, x2, y2 });
            if id == 0
                || !ids.insert(id)
                || candidate.is_none_or(|barrier| !valid_barrier(barrier, &zones))
            {
                failed.push(id);
            } else if let Some(candidate) = candidate {
                accepted.push(candidate);
            }
        }
        session.barriers = accepted;
        let mut results = HashMap::new();
        results.insert(
            "failed_barriers".into(),
            OwnedValue::try_from(zvariant::Value::from(failed)).expect("ids"),
        );
        (RESPONSE_SUCCESS, results)
    }

    fn enable(
        &self,
        session: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        self.set_enabled(OwnedObjectPath::from(session), app_id, true)
    }

    fn disable(
        &self,
        session: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        self.set_enabled(OwnedObjectPath::from(session), app_id, false)
    }

    fn release(
        &self,
        session: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.authenticated(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let path = OwnedObjectPath::from(session);
        if self.valid_session(&path, app_id).is_err() {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let activation_id = options
            .get("activation_id")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let generation = self
            .shared
            .0
            .lock()
            .unwrap()
            .sessions
            .get(&path)
            .map(|session| session.generation)
            .unwrap_or(0);
        let _ = self.sender.send(Message::ReleaseInputCapture {
            session: path,
            generation,
            activation_id,
        });
        (RESPONSE_SUCCESS, HashMap::new())
    }

    #[zbus(name = "ConnectToEIS")]
    fn connect_to_eis(
        &self,
        session: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.authenticated(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "not the portal frontend".into(),
            ));
        }
        let path = OwnedObjectPath::from(session);
        let (capabilities, generation) = {
            let mut state = self.shared.0.lock().unwrap();
            let entry = state
                .sessions
                .get_mut(&path)
                .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown session".into()))?;
            if entry.app_id != app_id || !entry.started || entry.capabilities == 0 || entry.eis {
                return Err(zbus::fdo::Error::AccessDenied(
                    "session is not ready for EIS".into(),
                ));
            }
            entry.eis = true;
            (entry.capabilities, entry.generation)
        };
        let (theirs, ours) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if self
            .sender
            .send(Message::ConnectInputCaptureEis {
                session: path.clone(),
                generation,
                stream: ours,
                capabilities,
            })
            .is_err()
        {
            if let Some(entry) = self
                .shared
                .0
                .lock()
                .unwrap()
                .sessions
                .get_mut(&path)
                .filter(|entry| entry.generation == generation)
            {
                entry.eis = false;
            }
            return Err(zbus::fdo::Error::Failed(
                "the compositor is not listening".into(),
            ));
        }
        Ok(zvariant::OwnedFd::from(OwnedFd::from(theirs)))
    }

    // Declared here for D-Bus introspection; compositor-side `Signals` emits
    // them on the same connection when input state changes.
    #[zbus(signal)]
    async fn disabled(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn activated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn zones_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}

pub fn valid_barrier(barrier: Barrier, zones: &[Zone]) -> bool {
    if barrier.x1 != barrier.x2 && barrier.y1 != barrier.y2 {
        return false;
    }
    if barrier.x1 == barrier.x2 && barrier.y1 == barrier.y2 {
        return false;
    }
    zones.iter().enumerate().any(|(index, zone)| {
        let left = zone.x;
        let right = zone.x.saturating_add_unsigned(zone.width);
        let top = zone.y;
        let bottom = zone.y.saturating_add_unsigned(zone.height);
        if barrier.x1 == barrier.x2 {
            let edge = barrier.x1 == left || barrier.x1 == right;
            let adjacent = zones.iter().enumerate().any(|(other_index, other)| {
                if index == other_index {
                    return false;
                }
                let other_left = other.x;
                let other_right = other.x.saturating_add_unsigned(other.width);
                let opposite = (barrier.x1 == left && other_right == barrier.x1)
                    || (barrier.x1 == right && other_left == barrier.x1);
                opposite
                    && ranges_overlap(
                        barrier.y1,
                        barrier.y2,
                        other.y,
                        other.y.saturating_add_unsigned(other.height),
                    )
            });
            edge && !adjacent
                && barrier.y1.min(barrier.y2) >= top
                && barrier.y1.max(barrier.y2) <= bottom
        } else {
            let edge = barrier.y1 == top || barrier.y1 == bottom;
            let adjacent = zones.iter().enumerate().any(|(other_index, other)| {
                if index == other_index {
                    return false;
                }
                let other_top = other.y;
                let other_bottom = other.y.saturating_add_unsigned(other.height);
                let opposite = (barrier.y1 == top && other_bottom == barrier.y1)
                    || (barrier.y1 == bottom && other_top == barrier.y1);
                opposite
                    && ranges_overlap(
                        barrier.x1,
                        barrier.x2,
                        other.x,
                        other.x.saturating_add_unsigned(other.width),
                    )
            });
            edge && !adjacent
                && barrier.x1.min(barrier.x2) >= left
                && barrier.x1.max(barrier.x2) <= right
        }
    })
}

fn ranges_overlap(a1: i32, a2: i32, b1: i32, b2: i32) -> bool {
    a1.min(a2) < b1.max(b2) && b1.min(b2) < a1.max(a2)
}

#[derive(Default)]
pub struct Connections {
    live: HashMap<OwnedObjectPath, Connection>,
    active: Option<Active>,
    next_activation: u32,
    ctrl: bool,
    alt: bool,
    down_keys: HashSet<u32>,
    down_buttons: HashSet<u32>,
    suppressed_keys: HashSet<u32>,
    suppressed_buttons: HashSet<u32>,
    modifiers: Option<smithay::input::keyboard::SerializedMods>,
}

struct Connection {
    generation: u64,
    token: RegistrationToken,
    connection: Option<reis::request::Connection>,
    devices: Vec<Device>,
    ready: HashSet<Device>,
    capabilities: u32,
}

#[derive(Clone)]
struct Active {
    session: OwnedObjectPath,
    generation: u64,
    id: u32,
    ignored_keys: HashSet<u32>,
    ignored_buttons: HashSet<u32>,
    captured_keys: HashSet<u32>,
    captured_buttons: HashSet<u32>,
    last_absolute: Option<Point<f64, Logical>>,
    modifiers: ModifiersState,
}

impl crate::state::ViewportState {
    pub fn refresh_input_capture_zones(&mut self) {
        let zones = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .filter_map(|geometry| {
                Some(Zone {
                    width: u32::try_from(geometry.size.w).ok()?,
                    height: u32::try_from(geometry.size.h).ok()?,
                    x: geometry.loc.x,
                    y: geometry.loc.y,
                })
            })
            .collect();
        let changed = self.input_capture_shared.update_zones(zones);
        if !changed.is_empty() {
            self.deactivate_input_capture();
            for session in changed {
                self.input_capture_signals.zones_changed(&session);
            }
        }
    }

    pub fn connect_input_capture_eis(
        &mut self,
        session: OwnedObjectPath,
        generation: u64,
        stream: std::os::unix::net::UnixStream,
        capabilities: u32,
    ) {
        if !self.input_capture_session_is(&session, generation, true) {
            return;
        }
        if self
            .input_capture_connections
            .active
            .as_ref()
            .is_some_and(|active| active.session == session)
        {
            self.deactivate_input_capture();
        }
        if let Some(previous) = self.input_capture_connections.live.remove(&session) {
            self.loop_handle.remove(previous.token);
        }
        let context = match eis::Context::new(stream) {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!("input capture: could not create EI context: {error}");
                if let Some(entry) = self
                    .input_capture_shared
                    .0
                    .lock()
                    .unwrap()
                    .sessions
                    .get_mut(&session)
                {
                    entry.eis = false;
                }
                return;
            }
        };
        let source = EisRequestSource::new(context, 1);
        let source_session = session.clone();
        let token = match self
            .loop_handle
            .insert_source(source, move |event, connection, state| {
                state.input_capture_eis_event(&source_session, event, connection)
            }) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!("input capture: could not watch EI socket: {error}");
                if let Some(entry) = self
                    .input_capture_shared
                    .0
                    .lock()
                    .unwrap()
                    .sessions
                    .get_mut(&session)
                {
                    entry.eis = false;
                }
                return;
            }
        };
        self.input_capture_connections.live.insert(
            session,
            Connection {
                generation,
                token,
                connection: None,
                devices: Vec::new(),
                ready: HashSet::new(),
                capabilities,
            },
        );
    }

    fn input_capture_eis_event(
        &mut self,
        session: &OwnedObjectPath,
        event: Result<EisRequestSourceEvent, reis::Error>,
        connection: &mut reis::request::Connection,
    ) -> std::io::Result<PostAction> {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!("input capture: EI protocol error: {error}");
                self.input_capture_disconnected(session);
                return Ok(PostAction::Remove);
            }
        };
        match event {
            EisRequestSourceEvent::Connected => {
                if connection.context_type() != eis::handshake::ContextType::Receiver {
                    connection.disconnected(
                        eis::connection::DisconnectReason::Protocol,
                        Some("InputCapture requires a receiver context"),
                    );
                    self.input_capture_disconnected(session);
                    return Ok(PostAction::Remove);
                }
                let capabilities = self
                    .input_capture_connections
                    .live
                    .get(session)
                    .map(|c| c.capabilities)
                    .unwrap_or(0);
                let mut flags = BitFlags::empty();
                if capabilities & KEYBOARD != 0 {
                    flags |= DeviceCapability::Keyboard;
                }
                if capabilities & POINTER != 0 {
                    flags |= DeviceCapability::Pointer
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll;
                }
                let _ = connection.add_seat(Some("Viewport input capture"), flags);
                if let Some(live) = self.input_capture_connections.live.get_mut(session) {
                    live.connection = Some(connection.clone());
                }
                connection.flush()?;
            }
            EisRequestSourceEvent::Request(EisRequest::Bind(bind)) => {
                let capabilities = self
                    .input_capture_connections
                    .live
                    .get(session)
                    .map(|c| c.capabilities)
                    .unwrap_or(0);
                let mut devices = Vec::new();
                if capabilities & POINTER != 0
                    && bind.capabilities.contains(DeviceCapability::Pointer)
                {
                    let mut flags: BitFlags<DeviceCapability> = DeviceCapability::Pointer.into();
                    if bind.capabilities.contains(DeviceCapability::Button) {
                        flags |= DeviceCapability::Button;
                    }
                    if bind.capabilities.contains(DeviceCapability::Scroll) {
                        flags |= DeviceCapability::Scroll;
                    }
                    let device = bind.seat.add_device(
                        Some("Viewport captured pointer"),
                        eis::device::DeviceType::Virtual,
                        flags,
                        |_| {},
                    );
                    device.resumed();
                    devices.push(device);
                }
                if capabilities & KEYBOARD != 0
                    && bind.capabilities.contains(DeviceCapability::Keyboard)
                {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let device = keyboard.with_xkb_state(self, |xkb| {
                            let keymap =
                                KeymapFile::new(unsafe { xkb.xkb().lock().unwrap().keymap() });
                            bind.seat.add_device(
                                Some("Viewport captured keyboard"),
                                eis::device::DeviceType::Virtual,
                                DeviceCapability::Keyboard.into(),
                                |device| {
                                    if let Some(interface) = device.interface::<eis::Keyboard>() {
                                        let _ = keymap.with_fd(false, |fd, size| {
                                            interface.keymap(
                                                eis::keyboard::KeymapType::Xkb,
                                                size as u32,
                                                fd,
                                            )
                                        });
                                    }
                                },
                            )
                        });
                        device.resumed();
                        devices.push(device);
                    }
                }
                if let Some(live) = self.input_capture_connections.live.get_mut(session) {
                    live.devices.extend(devices);
                }
                connection.flush()?;
            }
            EisRequestSourceEvent::Request(EisRequest::DeviceClosed(closed)) => {
                closed.device.remove();
                if let Some(live) = self.input_capture_connections.live.get_mut(session) {
                    live.devices.retain(|device| device != &closed.device);
                    live.ready.remove(&closed.device);
                }
                connection.flush()?;
            }
            EisRequestSourceEvent::Request(EisRequest::Ready(ready)) => {
                if let Some(live) = self.input_capture_connections.live.get_mut(session) {
                    live.ready.insert(ready.device.clone());
                }
                self.prime_input_capture_modifiers(session, &ready.device);
            }
            EisRequestSourceEvent::Request(EisRequest::Disconnect) => {
                self.input_capture_disconnected(session);
                return Ok(PostAction::Remove);
            }
            EisRequestSourceEvent::Request(_) => {}
        }
        Ok(PostAction::Continue)
    }

    fn input_capture_disconnected(&mut self, session: &OwnedObjectPath) {
        let generation = self
            .input_capture_connections
            .live
            .get(session)
            .map(|live| live.generation);
        if self
            .input_capture_connections
            .active
            .as_ref()
            .is_some_and(|active| &active.session == session)
        {
            self.deactivate_input_capture();
        }
        self.input_capture_connections.live.remove(session);
        if let Some(entry) = self
            .input_capture_shared
            .0
            .lock()
            .unwrap()
            .sessions
            .get_mut(session)
            .filter(|entry| Some(entry.generation) == generation)
        {
            entry.eis = false;
            entry.enabled = false;
        }
        self.input_capture_signals.disabled(session);
    }

    pub fn revoke_input_capture(
        &mut self,
        session: &OwnedObjectPath,
        generation: u64,
        remove_session: bool,
    ) {
        let shared_matches = self.input_capture_session_is(session, generation, false);
        let live_matches = self
            .input_capture_connections
            .live
            .get(session)
            .is_some_and(|live| live.generation == generation);
        if !shared_matches && !live_matches {
            return;
        }
        if self
            .input_capture_connections
            .active
            .as_ref()
            .is_some_and(|active| &active.session == session && active.generation == generation)
        {
            self.deactivate_input_capture();
        }
        if live_matches {
            if let Some(live) = self.input_capture_connections.live.remove(session) {
                self.loop_handle.remove(live.token);
            }
        }
        if remove_session {
            self.input_capture_shared.remove(session, generation);
        }
    }

    pub fn disable_input_capture(&mut self, session: &OwnedObjectPath, generation: u64) {
        if !self.input_capture_session_is(session, generation, false) {
            return;
        }
        if self
            .input_capture_connections
            .active
            .as_ref()
            .is_some_and(|active| &active.session == session && active.generation == generation)
        {
            self.deactivate_input_capture();
        }
        self.input_capture_signals.disabled(session);
    }

    pub fn suspend_input_capture(&mut self) {
        let sessions = {
            let mut shared = self.input_capture_shared.0.lock().unwrap();
            shared
                .sessions
                .iter_mut()
                .filter_map(|(path, session)| {
                    let was_enabled = session.enabled;
                    session.enabled = false;
                    was_enabled.then(|| path.clone())
                })
                .collect::<Vec<_>>()
        };
        self.deactivate_input_capture();
        for session in sessions {
            self.input_capture_signals.disabled(&session);
        }
    }

    pub fn release_input_capture(
        &mut self,
        session: &OwnedObjectPath,
        generation: u64,
        activation_id: u32,
    ) {
        if !self.input_capture_session_is(session, generation, false) {
            return;
        }
        if self
            .input_capture_connections
            .active
            .as_ref()
            .is_some_and(|active| {
                &active.session == session
                    && active.generation == generation
                    && active.id == activation_id
            })
        {
            self.deactivate_input_capture();
        }
    }

    fn input_capture_session_is(
        &self,
        session: &OwnedObjectPath,
        generation: u64,
        require_eis: bool,
    ) -> bool {
        self.input_capture_shared
            .0
            .lock()
            .unwrap()
            .sessions
            .get(session)
            .is_some_and(|entry| {
                session_generation_matches(entry, generation) && (!require_eis || entry.eis)
            })
    }

    pub fn deactivate_input_capture(&mut self) {
        let Some(active) = self.input_capture_connections.active.take() else {
            return;
        };
        self.input_capture_connections.modifiers = None;
        self.input_capture_connections
            .suppressed_keys
            .extend(active.captured_keys);
        self.input_capture_connections
            .suppressed_buttons
            .extend(active.captured_buttons);
        if let Some(live) = self
            .input_capture_connections
            .live
            .get(&active.session)
            .filter(|live| live.generation == active.generation)
        {
            for device in &live.ready {
                device.stop_emulating();
            }
            if let Some(connection) = &live.connection {
                let _ = connection.flush();
            }
        }
        let cursor = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .unwrap_or_default();
        self.input_capture_signals
            .deactivated(&active.session, active.id, cursor);
    }

    fn activate_input_capture(
        &mut self,
        session: OwnedObjectPath,
        barrier_id: u32,
        cursor: Point<f64, Logical>,
        last_absolute: Option<Point<f64, Logical>>,
    ) -> bool {
        let Some(live) = self.input_capture_connections.live.get(&session) else {
            return false;
        };
        if live.ready.is_empty() {
            return false;
        }
        let generation = live.generation;
        self.input_capture_connections.next_activation = self
            .input_capture_connections
            .next_activation
            .wrapping_add(1)
            .max(1);
        let id = self.input_capture_connections.next_activation;
        for device in &live.ready {
            device.start_emulating(id);
        }
        if let Some(connection) = &live.connection {
            let _ = connection.flush();
        }
        let modifiers = self
            .seat
            .get_keyboard()
            .map(|keyboard| keyboard.modifier_state())
            .unwrap_or_default();
        self.input_capture_connections.active = Some(Active {
            session: session.clone(),
            generation,
            id,
            ignored_keys: self.input_capture_connections.down_keys.clone(),
            ignored_buttons: self.input_capture_connections.down_buttons.clone(),
            captured_keys: HashSet::new(),
            captured_buttons: HashSet::new(),
            last_absolute,
            modifiers,
        });
        self.broadcast_input_capture_modifiers(modifiers.serialized);
        self.input_capture_signals
            .activated(&session, id, cursor, barrier_id);
        true
    }

    pub fn process_local_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        if self.intercept_input_capture(&event) {
            return;
        }
        self.process_input_event(event);
        self.sync_input_capture_modifiers();
    }

    fn intercept_input_capture<I: InputBackend>(&mut self, event: &InputEvent<I>) -> bool {
        if self.locked {
            self.deactivate_input_capture();
            return false;
        }
        if let InputEvent::Keyboard { event } = event {
            let code = event.key_code().raw().saturating_sub(8);
            let pressed = event.state() == KeyState::Pressed;
            if pressed {
                self.input_capture_connections.down_keys.insert(code);
            } else {
                self.input_capture_connections.down_keys.remove(&code);
                if self.input_capture_connections.suppressed_keys.remove(&code) {
                    return true;
                }
                if self
                    .input_capture_connections
                    .active
                    .as_mut()
                    .is_some_and(|active| active.ignored_keys.remove(&code))
                {
                    self.update_input_capture_key::<I>(event);
                    return false;
                }
            }
            match code {
                29 | 97 => self.input_capture_connections.ctrl = pressed,
                56 | 100 => self.input_capture_connections.alt = pressed,
                1 if pressed
                    && self.input_capture_connections.ctrl
                    && self.input_capture_connections.alt =>
                {
                    self.input_capture_connections.suppressed_keys.insert(code);
                    self.update_input_capture_key::<I>(event);
                    self.suspend_input_capture();
                    self.sync_input_capture_modifiers();
                    return true;
                }
                _ => {}
            }
        }
        if let InputEvent::PointerButton { event } = event {
            let button = event.button_code();
            let pressed = event.state() == ButtonState::Pressed;
            if pressed {
                self.input_capture_connections.down_buttons.insert(button);
            } else {
                self.input_capture_connections.down_buttons.remove(&button);
                if self
                    .input_capture_connections
                    .suppressed_buttons
                    .remove(&button)
                {
                    return true;
                }
                if self
                    .input_capture_connections
                    .active
                    .as_mut()
                    .is_some_and(|active| active.ignored_buttons.remove(&button))
                {
                    return false;
                }
            }
        }
        if self.input_capture_connections.active.is_none() {
            let Some(pointer) = self.seat.get_pointer() else {
                return false;
            };
            let from = pointer.current_location();
            let to = match event {
                InputEvent::PointerMotion { event } => from + event.delta(),
                InputEvent::PointerMotionAbsolute { event } => {
                    let Some(geometry) = self
                        .space
                        .outputs()
                        .next()
                        .and_then(|output| self.space.output_geometry(output))
                    else {
                        return false;
                    };
                    self.glass_to_content(
                        event.position_transformed(geometry.size) + geometry.loc.to_f64(),
                    )
                }
                _ => return false,
            };
            let trigger = {
                let shared = self.input_capture_shared.0.lock().unwrap();
                shared
                    .sessions
                    .iter()
                    .filter(|(_, session)| session.enabled && session.eis)
                    .flat_map(|(path, session)| {
                        session.barriers.iter().map(move |barrier| (path, barrier))
                    })
                    .filter(|(_, barrier)| crosses(**barrier, from, to))
                    .min_by_key(|(path, barrier)| (path.as_str().to_owned(), barrier.id))
                    .map(|(path, barrier)| (path.clone(), barrier.id))
            };
            let Some((session, barrier)) = trigger else {
                return false;
            };
            let last_absolute =
                matches!(event, InputEvent::PointerMotionAbsolute { .. }).then_some(from);
            if !self.activate_input_capture(session, barrier, to, last_absolute) {
                return false;
            }
        }
        let active = self
            .input_capture_connections
            .active
            .as_ref()
            .map(|active| active.session.clone());
        let Some(active) = active else { return false };
        let modifiers = match event {
            InputEvent::Keyboard { event } => self.update_input_capture_key::<I>(event),
            _ => None,
        };
        let Some(live) = self.input_capture_connections.live.get(&active) else {
            return false;
        };
        let capability = match event {
            InputEvent::Keyboard { .. } => DeviceCapability::Keyboard,
            InputEvent::PointerMotion { .. } | InputEvent::PointerMotionAbsolute { .. } => {
                DeviceCapability::Pointer
            }
            InputEvent::PointerButton { .. } => DeviceCapability::Button,
            InputEvent::PointerAxis { .. } => DeviceCapability::Scroll,
            _ => return false,
        };
        if !live
            .ready
            .iter()
            .any(|device| device.has_capability(capability))
        {
            return false;
        }
        let time = event_time_us(event);
        match event {
            InputEvent::Keyboard { event } => {
                let code = event.key_code().raw().saturating_sub(8);
                if let Some(active) = self.input_capture_connections.active.as_mut() {
                    if event.state() == KeyState::Pressed {
                        active.captured_keys.insert(code);
                    } else {
                        active.captured_keys.remove(&code);
                    }
                }
                if let Some(device) = live.devices.iter().find(|d| {
                    live.ready.contains(*d) && d.has_capability(DeviceCapability::Keyboard)
                }) {
                    if let Some(keyboard) = device.interface::<eis::Keyboard>() {
                        let state = if event.state() == KeyState::Pressed {
                            eis::keyboard::KeyState::Press
                        } else {
                            eis::keyboard::KeyState::Released
                        };
                        keyboard.key(code, state);
                        device.frame(time);
                    }
                }
            }
            InputEvent::PointerMotion { event } => {
                if let Some(device) = live.devices.iter().find(|d| {
                    live.ready.contains(*d) && d.has_capability(DeviceCapability::Pointer)
                }) {
                    if let Some(pointer) = device.interface::<eis::Pointer>() {
                        let delta = event.delta();
                        pointer.motion_relative(delta.x as f32, delta.y as f32);
                        device.frame(time);
                    }
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let output_geometry = self
                    .space
                    .outputs()
                    .next()
                    .and_then(|output| self.space.output_geometry(output));
                if let Some(geometry) = output_geometry {
                    let to = self.glass_to_content(
                        event.position_transformed(geometry.size) + geometry.loc.to_f64(),
                    );
                    let cursor = self
                        .seat
                        .get_pointer()
                        .map(|pointer| pointer.current_location())
                        .unwrap_or(to);
                    let from = self
                        .input_capture_connections
                        .active
                        .as_mut()
                        .and_then(|active| active.last_absolute.replace(to))
                        .unwrap_or(cursor);
                    let delta = to - from;
                    if let Some(device) = live.devices.iter().find(|d| {
                        live.ready.contains(*d) && d.has_capability(DeviceCapability::Pointer)
                    }) {
                        if let Some(pointer) = device.interface::<eis::Pointer>() {
                            pointer.motion_relative(delta.x as f32, delta.y as f32);
                            device.frame(time);
                        }
                    }
                }
            }
            InputEvent::PointerButton { event } => {
                let code = event.button_code();
                if let Some(active) = self.input_capture_connections.active.as_mut() {
                    if event.state() == ButtonState::Pressed {
                        active.captured_buttons.insert(code);
                    } else {
                        active.captured_buttons.remove(&code);
                    }
                }
                if let Some(device) = live
                    .devices
                    .iter()
                    .find(|d| live.ready.contains(*d) && d.has_capability(DeviceCapability::Button))
                {
                    if let Some(button) = device.interface::<eis::Button>() {
                        let state = if event.state() == ButtonState::Pressed {
                            eis::button::ButtonState::Press
                        } else {
                            eis::button::ButtonState::Released
                        };
                        button.button(code, state);
                        device.frame(time);
                    }
                }
            }
            InputEvent::PointerAxis { event } => {
                if let Some(device) = live
                    .devices
                    .iter()
                    .find(|d| live.ready.contains(*d) && d.has_capability(DeviceCapability::Scroll))
                {
                    if let Some(scroll) = device.interface::<eis::Scroll>() {
                        let horizontal = event.amount(Axis::Horizontal).unwrap_or(0.0);
                        let vertical = event.amount(Axis::Vertical).unwrap_or(0.0);
                        if horizontal != 0.0 || vertical != 0.0 {
                            scroll.scroll(horizontal as f32, vertical as f32);
                        }
                        if event.source() == AxisSource::Wheel {
                            scroll.scroll_discrete(
                                event.amount_v120(Axis::Horizontal).unwrap_or(0.0) as i32,
                                event.amount_v120(Axis::Vertical).unwrap_or(0.0) as i32,
                            );
                        } else {
                            let (horizontal, vertical) = scroll_stop_axes(
                                event.source(),
                                event.amount(Axis::Horizontal),
                                event.amount(Axis::Vertical),
                            );
                            if horizontal || vertical {
                                scroll.scroll_stop(u32::from(horizontal), u32::from(vertical), 0);
                            }
                        }
                        device.frame(time);
                    }
                }
            }
            _ => return false,
        }
        if let Some(connection) = &live.connection {
            let _ = connection.flush();
        }
        if let Some(modifiers) = modifiers {
            self.broadcast_input_capture_modifiers(modifiers);
        }
        true
    }

    fn update_input_capture_key<I: InputBackend>(
        &mut self,
        event: &I::KeyboardKeyEvent,
    ) -> Option<SerializedMods> {
        let keyboard = self.seat.get_keyboard()?;
        let local = keyboard.modifier_state();
        let capture = self
            .input_capture_connections
            .active
            .as_ref()
            .map(|active| active.modifiers)?;
        keyboard.set_modifier_state(capture);
        keyboard.input::<(), _>(
            self,
            event.key_code(),
            event.state(),
            SERIAL_COUNTER.next_serial(),
            event.time(),
            |_, _, _| FilterResult::Intercept(()),
        );
        let current = keyboard.modifier_state();
        keyboard.set_modifier_state(local);
        if let Some(active) = self.input_capture_connections.active.as_mut() {
            active.modifiers = current;
        }
        (current.serialized != capture.serialized).then_some(current.serialized)
    }

    fn prime_input_capture_modifiers(&mut self, session: &OwnedObjectPath, device: &Device) {
        if !device.has_capability(DeviceCapability::Keyboard) {
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let modifiers = keyboard.modifier_state().serialized;
        let Some(live) = self.input_capture_connections.live.get(session) else {
            return;
        };
        if let Some(interface) = device.interface::<eis::Keyboard>() {
            send_keyboard_modifiers(live.connection.as_ref(), &interface, modifiers);
            if let Some(connection) = &live.connection {
                let _ = connection.flush();
            }
        }
        self.input_capture_connections.modifiers = Some(modifiers);
    }

    fn sync_input_capture_modifiers(&mut self) {
        let modifiers = if let Some(active) = self.input_capture_connections.active.as_ref() {
            active.modifiers.serialized
        } else {
            let Some(keyboard) = self.seat.get_keyboard() else {
                return;
            };
            keyboard.modifier_state().serialized
        };
        self.broadcast_input_capture_modifiers(modifiers);
    }

    fn broadcast_input_capture_modifiers(&mut self, modifiers: SerializedMods) {
        if self.input_capture_connections.modifiers == Some(modifiers) {
            return;
        }
        self.input_capture_connections.modifiers = Some(modifiers);
        for live in self.input_capture_connections.live.values() {
            for device in live
                .ready
                .iter()
                .filter(|device| device.has_capability(DeviceCapability::Keyboard))
            {
                if let Some(interface) = device.interface::<eis::Keyboard>() {
                    send_keyboard_modifiers(live.connection.as_ref(), &interface, modifiers);
                }
            }
            if let Some(connection) = &live.connection {
                let _ = connection.flush();
            }
        }
    }
}

fn send_keyboard_modifiers(
    connection: Option<&reis::request::Connection>,
    keyboard: &eis::Keyboard,
    modifiers: SerializedMods,
) {
    let Some(connection) = connection else { return };
    let (depressed, locked, latched, group) = modifier_args(modifiers);
    connection
        .with_next_serial(|serial| keyboard.modifiers(serial, depressed, locked, latched, group));
}

fn modifier_args(modifiers: SerializedMods) -> (u32, u32, u32, u32) {
    (
        modifiers.depressed,
        modifiers.locked,
        modifiers.latched,
        modifiers.layout_effective,
    )
}

fn scroll_stop_axes(
    source: AxisSource,
    horizontal: Option<f64>,
    vertical: Option<f64>,
) -> (bool, bool) {
    (
        source == AxisSource::Finger && horizontal == Some(0.0),
        source == AxisSource::Finger && vertical == Some(0.0),
    )
}

fn session_generation_matches(session: &Session, generation: u64) -> bool {
    session.generation == generation
}

fn event_time_us<I: InputBackend>(event: &InputEvent<I>) -> u64 {
    match event {
        InputEvent::Keyboard { event } => event.time().micros(),
        InputEvent::PointerMotion { event } => event.time().micros(),
        InputEvent::PointerMotionAbsolute { event } => event.time().micros(),
        InputEvent::PointerButton { event } => event.time().micros(),
        InputEvent::PointerAxis { event } => event.time().micros(),
        _ => 0,
    }
}

fn crosses(barrier: Barrier, from: Point<f64, Logical>, to: Point<f64, Logical>) -> bool {
    if barrier.x1 == barrier.x2 {
        let x = f64::from(barrier.x1);
        (from.x - x) * (to.x - x) <= 0.0
            && from.x != to.x
            && between(
                intersection(from.y, to.y, from.x, to.x, x),
                barrier.y1,
                barrier.y2,
            )
    } else {
        let y = f64::from(barrier.y1);
        (from.y - y) * (to.y - y) <= 0.0
            && from.y != to.y
            && between(
                intersection(from.x, to.x, from.y, to.y, y),
                barrier.x1,
                barrier.x2,
            )
    }
}

fn intersection(a: f64, b: f64, from: f64, to: f64, line: f64) -> f64 {
    a + (b - a) * ((line - from) / (to - from))
}

fn between(value: f64, a: i32, b: i32) -> bool {
    value >= f64::from(a.min(b)) && value <= f64::from(a.max(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONE: Zone = Zone {
        width: 100,
        height: 80,
        x: 10,
        y: 20,
    };

    #[test]
    fn barriers_must_be_non_diagonal_output_edges() {
        assert!(valid_barrier(
            Barrier {
                id: 1,
                x1: 110,
                y1: 20,
                x2: 110,
                y2: 100
            },
            &[ZONE]
        ));
        assert!(!valid_barrier(
            Barrier {
                id: 1,
                x1: 50,
                y1: 20,
                x2: 50,
                y2: 100
            },
            &[ZONE]
        ));

        let staggered = Zone {
            x: 110,
            y: 20,
            width: 100,
            height: 40,
        };
        assert!(valid_barrier(
            Barrier {
                id: 2,
                x1: 110,
                y1: 60,
                x2: 110,
                y2: 100,
            },
            &[ZONE, staggered],
        ));
        let adjacent = Zone { x: 110, ..ZONE };
        assert!(!valid_barrier(
            Barrier {
                id: 1,
                x1: 110,
                y1: 20,
                x2: 110,
                y2: 100,
            },
            &[ZONE, adjacent],
        ));
        assert!(!valid_barrier(
            Barrier {
                id: 1,
                x1: 10,
                y1: 20,
                x2: 110,
                y2: 100
            },
            &[ZONE]
        ));
    }

    #[test]
    fn crossing_requires_motion_through_segment() {
        let barrier = Barrier {
            id: 1,
            x1: 100,
            y1: 0,
            x2: 100,
            y2: 80,
        };
        assert!(crosses(barrier, (90.0, 40.0).into(), (110.0, 40.0).into()));
        assert!(!crosses(barrier, (90.0, 90.0).into(), (110.0, 90.0).into()));
    }

    #[test]
    fn stale_session_generations_do_not_match_reused_paths() {
        let path = OwnedObjectPath::try_from("/org/example/session").unwrap();
        let shared = Shared::default();
        let session = Session {
            generation: 7,
            app_id: "org.example.App".into(),
            started: true,
            capabilities: SUPPORTED_CAPABILITIES,
            enabled: true,
            eis: true,
            barriers: Vec::new(),
        };
        assert!(session_generation_matches(&session, 7));
        assert!(!session_generation_matches(&session, 6));
        shared
            .0
            .lock()
            .unwrap()
            .sessions
            .insert(path.clone(), session);
        shared.remove(&path, 6);
        assert!(shared.0.lock().unwrap().sessions.contains_key(&path));
        shared.remove(&path, 7);
        assert!(!shared.0.lock().unwrap().sessions.contains_key(&path));
    }

    #[test]
    fn keyboard_modifier_arguments_use_ei_order() {
        assert_eq!(
            modifier_args(SerializedMods {
                depressed: 1,
                latched: 2,
                locked: 4,
                layout_effective: 8,
            }),
            (1, 4, 2, 8)
        );
    }

    #[test]
    fn only_finger_zero_deltas_stop_scroll_axes() {
        assert_eq!(
            scroll_stop_axes(AxisSource::Finger, Some(0.0), Some(3.0)),
            (true, false)
        );
        assert_eq!(
            scroll_stop_axes(AxisSource::Wheel, Some(0.0), Some(0.0)),
            (false, false)
        );
    }

    #[test]
    fn reis_accepts_a_receiver_context() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = std::thread::spawn(move || {
            reis::ei::Context::new(client)
                .unwrap()
                .handshake_blocking(
                    "input-capture-test",
                    reis::ei::handshake::ContextType::Receiver,
                )
                .map(|_| ())
        });
        let context = eis::Context::new(server).unwrap();
        let source = EisRequestSource::new(context, 1);
        let mut event_loop = smithay::reexports::calloop::EventLoop::<bool>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(source, |event, connection, connected| {
                if matches!(event, Ok(EisRequestSourceEvent::Connected)) {
                    assert_eq!(
                        connection.context_type(),
                        eis::handshake::ContextType::Receiver
                    );
                    let _ =
                        connection.add_seat(Some("test"), BitFlags::<DeviceCapability>::empty());
                    connection.flush()?;
                    *connected = true;
                }
                Ok(PostAction::Continue)
            })
            .unwrap();
        let mut connected = false;
        while !connected {
            event_loop
                .dispatch(std::time::Duration::from_secs(1), &mut connected)
                .unwrap();
        }
        client.join().unwrap().unwrap();
    }
}
