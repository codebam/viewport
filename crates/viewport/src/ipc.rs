// SPDX-License-Identifier: GPL-3.0-or-later
//
// The control socket. Ports the UNIX socket transport at src/ipc.c:1599.
//
// A newline-delimited JSON socket at $XDG_RUNTIME_DIR/viewport-<display>.sock,
// named after the Wayland display rather than the pid: both are unique per
// session, but only one is discoverable by a script that already has
// WAYLAND_DISPLAY.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};

use viewport_ipc::{Event, ParseError, Request};

use crate::framing::{Framed, Framer};
use crate::state::ViewportState;

/// A `UnixStream` that can be both owned by a calloop source and written to
/// from the compositor state.
///
/// The source needs the fd to poll and the broadcast path needs the stream to
/// write to, and calloop's `Generic` takes ownership of what it polls.
struct Shared(Rc<UnixStream>);

impl AsFd for Shared {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

struct Client {
    stream: Rc<UnixStream>,
    framer: Framer,
    /// What a short write left behind. Nothing else will send it, so the
    /// writable half of the source has to.
    pending: Vec<u8>,
    token: RegistrationToken,
    dead: bool,
}

pub struct Ipc {
    path: PathBuf,
    clients: HashMap<u64, Client>,
    next_client: u64,
}

impl Ipc {
    /// `$XDG_RUNTIME_DIR/viewport-<display>.sock`, falling back to `/tmp` and
    /// then to the pid, exactly as `src/ipc.c:1660` does.
    pub fn default_path(display: Option<&str>) -> PathBuf {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        match display {
            Some(display) => PathBuf::from(format!("{dir}/viewport-{display}.sock")),
            None => PathBuf::from(format!("{dir}/viewport-{}.sock", std::process::id())),
        }
    }

    pub fn new(
        path: PathBuf,
        loop_handle: &LoopHandle<'static, ViewportState>,
    ) -> Result<Self> {
        // sockaddr_un.sun_path is 108 bytes on Linux including the terminator.
        // Checking here rather than letting bind() fail turns "path must be
        // shorter than SUN_LEN" into something that names the path
        // (`src/ipc.c:1693`).
        const SUN_LEN: usize = 107;
        let len = path.as_os_str().as_encoded_bytes().len();
        anyhow::ensure!(
            len <= SUN_LEN,
            "control socket path too long ({len} > {SUN_LEN} bytes): {}",
            path.display()
        );

        // A stale socket from a compositor that did not exit cleanly would
        // otherwise make bind() fail with EADDRINUSE forever.
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind {}", path.display()))?;
        listener.set_nonblocking(true)?;

        // bind() creates the node world-accessible and the chmod only narrows
        // it afterwards, so there is a window in which anyone can connect.
        // XDG_RUNTIME_DIR is 0700 and hides it, but the /tmp fallback is not.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;

        loop_handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state| {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => state.ipc_accept(stream),
                            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                            Err(e) => {
                                tracing::warn!("control socket accept: {e}");
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("insert control socket source: {e}"))?;

        // For anything that would rather not assemble the path itself.
        unsafe { std::env::set_var("VIEWPORT_SOCKET", &path) };
        tracing::info!("control socket at {}", path.display());

        Ok(Self {
            path,
            clients: HashMap::new(),
            next_client: 1,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Send to every connected client.
    ///
    /// Note there is no origin filtering: a Viewport client is not one of
    /// several peers, it is the shell drawing the desktop, and everything it
    /// needs to know it needs to know on the channel it already listens to.
    pub fn broadcast(&mut self, event: &Event) {
        let Ok(mut text) = viewport_ipc::to_string(event) else {
            tracing::error!("could not serialise {event:?}");
            return;
        };
        text.push('\n');
        for client in self.clients.values_mut() {
            client.send(text.as_bytes());
        }
    }

    /// Send to one client, for an error that belongs to its sender.
    pub fn send_to(&mut self, client_id: u64, event: &Event) {
        let Ok(mut text) = viewport_ipc::to_string(event) else {
            return;
        };
        text.push('\n');
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.send(text.as_bytes());
        }
    }

    fn reap(&mut self, loop_handle: &LoopHandle<'static, ViewportState>) {
        let dead: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.dead)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            if let Some(client) = self.clients.remove(&id) {
                loop_handle.remove(client.token);
            }
        }
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Client {
    fn send(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.flush();
    }

    fn flush(&mut self) {
        while !self.pending.is_empty() {
            match (&*self.stream).write(&self.pending) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

impl ViewportState {
    fn ipc_accept(&mut self, stream: UnixStream) {
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let stream = Rc::new(stream);
        let id = self.ipc.next_client;
        self.ipc.next_client += 1;

        let source = Generic::new(Shared(stream.clone()), Interest::BOTH, Mode::Level);
        let token = match self.loop_handle.insert_source(source, move |readiness, shared, state| {
            if readiness.writable {
                if let Some(client) = state.ipc.clients.get_mut(&id) {
                    client.flush();
                }
            }
            if readiness.readable {
                state.ipc_read(id, &shared.0);
            }
            state.ipc.reap(&state.loop_handle.clone());
            Ok(PostAction::Continue)
        }) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!("could not register control client: {e}");
                return;
            }
        };

        self.ipc.clients.insert(
            id,
            Client {
                stream,
                framer: Framer::new(),
                pending: Vec::new(),
                token,
                dead: false,
            },
        );
    }

    fn ipc_read(&mut self, id: u64, stream: &UnixStream) {
        let mut chunk = [0u8; 4096];
        loop {
            let n = match (&*stream).read(&mut chunk) {
                Ok(0) => {
                    self.ipc_kill(id);
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.ipc_kill(id);
                    return;
                }
            };

            let Some(client) = self.ipc.clients.get_mut(&id) else {
                return;
            };
            let messages = match client.framer.push(&chunk[..n]) {
                Framed::Messages(messages) => messages,
                Framed::Overrun => {
                    tracing::warn!("control client {id} overran the accumulator");
                    self.ipc_kill(id);
                    return;
                }
            };

            for message in messages {
                self.ipc_dispatch(id, &message);
                // Handling a message can drop this very client.
                if !self.ipc.clients.contains_key(&id) {
                    return;
                }
            }
        }
    }

    fn ipc_kill(&mut self, id: u64) {
        if let Some(client) = self.ipc.clients.get_mut(&id) {
            client.dead = true;
        }
    }

    /// Drain everything the page has posted and act on it.
    ///
    /// Called from the event loop rather than from the callback, because the
    /// callback runs underneath a dispatch that already holds this state.
    #[cfg(feature = "wpe")]
    pub fn drain_shell(&mut self) {
        let Some(shell) = self.shell.as_ref() else {
            return;
        };
        for message in shell.take_messages() {
            // Client id 0: the shell is not one of the socket clients, and an
            // error it caused goes to the broadcast channel it already
            // listens to rather than to a connection that does not exist.
            self.ipc_dispatch(0, message.as_bytes());
        }
    }

    /// Parse one message and act on it.
    pub fn ipc_dispatch(&mut self, client_id: u64, bytes: &[u8]) {
        // The first message the shell sends, once.
        //
        // "The shell did not lay anything out" has two very different causes:
        // the page never ran, or it ran and its layout was wrong. Nothing else
        // in the log distinguishes them.
        if !self.shell_announced {
            self.shell_announced = true;
            tracing::info!("shell is talking to us");
        }

        match viewport_ipc::parse(bytes) {
            Ok(request) => self.handle_request(request),
            Err(error) => {
                tracing::debug!("rejected IPC message: {error}");
                self.ipc_reject(client_id, &error);
            }
        }
    }

    fn ipc_reject(&mut self, client_id: u64, error: &ParseError) {
        let event = error.to_event();
        // An error belongs to its sender. Broadcasting would tell every other
        // client about a mistake it did not make — except for the shell,
        // which has no connection of its own and must hear about its own
        // mistakes on the channel it does listen to.
        if client_id == 0 {
            self.notify(&event);
        } else {
            self.ipc.send_to(client_id, &event);
        }
    }

    /// Act on a parsed message. Split out so tests can drive the compositor
    /// without a socket.
    pub fn handle_request(&mut self, request: Request) {
        crate::apply::apply(self, request);
    }
}
