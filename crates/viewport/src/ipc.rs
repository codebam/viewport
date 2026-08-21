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
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
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

/// How much unsent event text one client may be owed before it is suspected of
/// not reading at all.
///
/// The mirror of `framing::MAX_PENDING`, which bounds what a client may send
/// us: a connection that reads nothing while `subscribe` pours events at it
/// otherwise grows the compositor's heap without limit, because a write that
/// comes back `WouldBlock` leaves everything behind and nothing ever takes it.
/// A megabyte is hundreds of events — far more than a reader that is merely
/// slow falls behind by.
const MAX_BACKLOG: usize = 1 << 20;

/// How long a client may sit above [`MAX_BACKLOG`] taking none of it.
///
/// Size alone was the test here, and it was the wrong one: a backlog says how
/// much a client has been sent, not whether it is reading. One event can be
/// over the line by itself — a tray item's 512-pixel icon was 1.4MB of data
/// URL — and the shell, which reads everything it is sent within a frame, was
/// dropped for a message it had not been given a chance to take yet. The
/// desktop went with it.
///
/// So what is measured is progress. A reader handed a burst it has to work
/// through drains some of it every pass through the loop and resets this; a
/// reader that has stopped moves nothing, and after five seconds of moving
/// nothing it is gone whatever its socket still says.
const STUCK: std::time::Duration = std::time::Duration::from_secs(5);

/// The backlog no client is given the benefit of the doubt over.
///
/// [`STUCK`] bounds how long the compositor waits, and this bounds what that
/// wait may cost it in the meantime: a client that has genuinely stopped while
/// something floods it with events would otherwise take the compositor's heap
/// with it before its five seconds were up.
const HARD_BACKLOG: usize = 64 << 20;

struct Client {
    stream: Rc<UnixStream>,
    /// The process on the other end, when the kernel will say.
    ///
    /// What makes a shell's connection recognisable on a socket every client
    /// may open. The shell is spawned by this compositor, so its pid is known
    /// here — and a pid read from `SO_PEERCRED` is the kernel's answer, not the
    /// client's, so nothing can claim to be the desktop by saying so.
    pid: Option<i32>,
    framer: Framer,
    /// What a short write left behind. Nothing else will send it, so the
    /// writable half of the source has to.
    ///
    /// Bounded by [`MAX_BACKLOG`] and [`STUCK`] together: a subscriber that
    /// stops reading is not a reason for the compositor to grow.
    pending: Vec<u8>,
    /// When this client last had a backlog over [`MAX_BACKLOG`] and took none
    /// of it, or `None` while it is keeping up.
    ///
    /// Cleared by any successful write, so the clock measures a reader that is
    /// stuck rather than one that is busy.
    stalled_since: Option<std::time::Instant>,
    token: RegistrationToken,
    /// The write-readiness source, which exists only while `pending` does.
    ///
    /// A connected socket with an empty send buffer is writable at all times,
    /// so a level-triggered source that asks about writability is ready on
    /// every pass through the loop whether or not there is anything to write.
    /// Registering both halves for the life of the connection therefore meant
    /// the compositor never slept while anything was connected: an idle
    /// session went from 15% of a core to 101% the moment a client opened the
    /// socket, with no message sent — and Viewport's own tooling holds exactly
    /// that kind of connection.
    ///
    /// So writability is asked about only when there is something to write.
    /// This is inserted when a write comes up short and removes itself once
    /// the backlog is gone.
    write_token: Option<RegistrationToken>,
    dead: bool,
}

pub struct Ipc {
    path: PathBuf,
    clients: HashMap<u64, Client>,
    next_client: u64,
    /// For arming a client's write source from the send path, which is
    /// reached from ordinary compositor code and not only from a callback.
    loop_handle: LoopHandle<'static, ViewportState>,
}

/// A fresh 0700 directory beside the socket's final home, for it to be born in.
///
/// Named after the pid and the clock, mkdtemp style, and created
/// fail-if-exists so a directory somebody else planted at a guessed name is
/// an error to retry around rather than a place to bind. The name only has to
/// avoid collisions, not keep secrets — the mode is the boundary.
fn staging_dir(parent: &std::path::Path) -> Result<PathBuf> {
    let mut last = None;
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = parent.join(format!(".viewport-{}-{nanos:x}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) => last = Some((dir, e)),
        }
    }
    let (dir, e) = last.expect("the loop ran at least once");
    Err(e).with_context(|| format!("creating {}", dir.display()))
}

/// Bind the listener at `staged`, close it to the world there, and move it
/// onto `final_path`.
///
/// Everything between "a socket exists" and "the socket is announced" happens
/// inside the 0700 directory `staged` lives in, which is what makes the order
/// safe: while the node may still be world-accessible, nobody else can walk
/// to it. The rename is what announces it, already 0600 — rename(2) moves the
/// inode with its permissions and replaces whatever sits at the destination,
/// which also covers a stale socket that appeared after the cleanup above.
fn listen_at(staged: &std::path::Path, final_path: &std::path::Path) -> Result<UnixListener> {
    let listener =
        UnixListener::bind(staged).with_context(|| format!("bind {}", staged.display()))?;

    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", staged.display()))?;

    std::fs::rename(staged, final_path)
        .with_context(|| format!("rename {} to {}", staged.display(), final_path.display()))?;
    Ok(listener)
}

impl Ipc {
    /// Where it is listening.
    ///
    /// The out-of-process shell is told this outright rather than deriving it:
    /// it is started with `WAYLAND_SOCKET` and so has no `WAYLAND_DISPLAY` to
    /// derive it from.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// `$XDG_RUNTIME_DIR/viewport-<display>.sock`, falling back to `/tmp` and
    /// then to the pid, exactly as `src/ipc.c:1660` does.
    pub fn default_path(display: Option<&str>) -> PathBuf {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        match display {
            Some(display) => PathBuf::from(format!("{dir}/viewport-{display}.sock")),
            None => PathBuf::from(format!("{dir}/viewport-{}.sock", std::process::id())),
        }
    }

    pub fn new(path: PathBuf, loop_handle: &LoopHandle<'static, ViewportState>) -> Result<Self> {
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

        // bind() creates the node world-accessible and a chmod narrows it only
        // afterwards, so binding straight onto the final name leaves a window
        // — however short — in which anyone can connect, and what sits behind
        // this socket includes running `/bin/sh -c` on client text.
        // XDG_RUNTIME_DIR is 0700 and hides the window; the /tmp fallback this
        // keeps for parity with the C build (`src/ipc.c:1660`) is world-walkable
        // and does not.
        //
        // So the listener is born somewhere private instead. A fresh 0700
        // directory is made beside the final path, the socket binds inside it,
        // and it is chmod 0600 while it is still in there — unreachable by
        // anybody else whatever its mode, because nobody else can walk the
        // directory to reach it. Only then is it renamed onto the well-known
        // name. rename(2) moves the inode with its permissions, so the socket
        // clients look for appears already closed to the world: there is no
        // window left to narrow after the fact.
        //
        // The advertised path deliberately stays the well-known
        // `$XDG_RUNTIME_DIR/viewport-<display>.sock` rather than moving inside
        // the private directory. Every client derives that shape
        // independently — msg.rs's discovery, the scripts, the documentation,
        // and the C build this ports — and a per-session directory would have
        // to be taught to all of them for a property, predictability, they
        // depend on. Renaming within the same parent keeps the well-known name
        // exactly as it was and closes the race all the same; being under the
        // same parent also keeps the rename on one filesystem, which is the
        // only way it can work.
        let filename = path
            .file_name()
            .with_context(|| format!("control socket path has no file name: {}", path.display()))?
            .to_owned();
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let staged = staging_dir(&parent)?.join(&filename);
        // The staging path is longer than the final one and has to fit
        // sun_path too, or bind() fails where clients would have been fine.
        let staged_len = staged.as_os_str().as_encoded_bytes().len();
        anyhow::ensure!(
            staged_len <= SUN_LEN,
            "control socket path too long ({staged_len} > {SUN_LEN} bytes): {}",
            staged.display()
        );

        let listener = match listen_at(&staged, &path) {
            Ok(listener) => listener,
            Err(e) => {
                // Nothing at the well-known name was touched: whatever failed
                // failed inside the private directory, which is now litter.
                let _ = std::fs::remove_dir_all(staged.parent().expect("the staging dir"));
                return Err(e);
            }
        };
        // Emptied by the rename, so this takes the directory and nothing else.
        if let Some(dir) = staged.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        listener.set_nonblocking(true)?;

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
            loop_handle: loop_handle.clone(),
        })
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
        self.arm_writers();
    }

    /// The process on the other end of a connection, if the kernel said.
    pub fn client_pid(&self, client_id: u64) -> Option<i32> {
        self.clients.get(&client_id).and_then(|client| client.pid)
    }

    /// The connection a process holds, if it has one.
    pub fn client_for_pid(&self, pid: i32) -> Option<u64> {
        self.clients
            .iter()
            .find(|(_, client)| client.pid == Some(pid))
            .map(|(id, _)| *id)
    }

    /// Send to everything except the processes named.
    ///
    /// For an event that has a different answer per shell: each of them is sent
    /// its own, and this is what carries the plain one to everyone else. A
    /// script watching the socket is told what the machine is; a page is told
    /// what it covers.
    pub fn broadcast_except(&mut self, pids: &[i32], event: &Event) {
        let Ok(mut text) = viewport_ipc::to_string(event) else {
            tracing::error!("could not serialise {event:?}");
            return;
        };
        text.push('\n');
        for client in self.clients.values_mut() {
            if client.pid.is_some_and(|pid| pids.contains(&pid)) {
                continue;
            }
            client.send(text.as_bytes());
        }
        self.arm_writers();
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
        self.arm_writer(client_id);
    }

    /// Close every connection, before the process that owns them goes away.
    ///
    /// Shutting the sockets down rather than dropping the clients, because a
    /// stream is held by its calloop sources as well as by the map here and a
    /// drop only closes the last handle. `shutdown` acts on the connection
    /// itself, so the peer reads end-of-file whoever else is still holding it.
    ///
    /// What this buys is the order at quit. An out-of-process shell watches
    /// this socket and stops its engine when it closes, so closing it first
    /// and waiting means the engine exits while the Wayland display is still
    /// there. Exiting the other way round is what made a quit noisy: the
    /// display went first, servoshell took its broken-pipe path, and died in
    /// its own exit handlers on the way out.
    pub fn close_all(&mut self) {
        for client in self.clients.values_mut() {
            let _ = client.stream.shutdown(std::net::Shutdown::Both);
            client.dead = true;
        }
    }

    /// Ask about writability for any client a write came up short on.
    ///
    /// The `any` first so that the ordinary case — every write completed,
    /// which is nearly all of them — costs a scan and no allocation.
    fn arm_writers(&mut self) {
        if !self.clients.values().any(Client::wants_writable) {
            return;
        }
        let ids: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, client)| client.wants_writable())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.arm_writer(id);
        }
    }

    fn arm_writer(&mut self, id: u64) {
        // A clone, so the borrow of `self.clients` below does not collide with
        // the loop handle this inserts through.
        let loop_handle = self.loop_handle.clone();
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if !client.wants_writable() {
            return;
        }

        let source = Generic::new(Shared(client.stream.clone()), Interest::WRITE, Mode::Level);
        let token = loop_handle.insert_source(source, move |_, _, state: &mut ViewportState| {
            // Drained, or gone: either way this source has no further job, and
            // leaving it registered would be the busy loop it exists to avoid.
            let mut finished = true;
            if let Some(client) = state.ipc.clients.get_mut(&id) {
                client.flush();
                finished = client.dead || client.pending.is_empty();
                if finished {
                    client.write_token = None;
                }
            }
            // Not reaping here: a dead client is removed by the read half,
            // which sees the same hangup, and removing this source twice —
            // once by returning `Remove` and once through `reap` — is not
            // something calloop forgives.
            Ok(if finished {
                PostAction::Remove
            } else {
                PostAction::Continue
            })
        });

        match token {
            Ok(token) => client.write_token = Some(token),
            Err(e) => {
                tracing::warn!("could not watch control client {id} for writability: {e}");
                client.dead = true;
            }
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
                if let Some(token) = client.write_token {
                    loop_handle.remove(token);
                }
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
    /// Whether this client has a backlog and nothing watching for the chance
    /// to clear it.
    fn wants_writable(&self) -> bool {
        !self.dead && !self.pending.is_empty() && self.write_token.is_none()
    }

    fn send(&mut self, bytes: &[u8]) {
        if self.dead {
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.flush();

        if self.pending.len() <= MAX_BACKLOG {
            self.stalled_since = None;
            return;
        }
        // Over the line. Same rule as `Framed::Overrun` on the read side — a
        // client that has stopped taking what it asked for is gone — but only
        // once it has had [`STUCK`] to take any of it, because being sent a lot
        // at once is not the same as reading none of it.
        let since = *self
            .stalled_since
            .get_or_insert_with(std::time::Instant::now);
        if self.pending.len() > HARD_BACKLOG || since.elapsed() > STUCK {
            tracing::warn!(
                "control client is {} bytes behind and has taken none of it for {:.1}s; \
                 dropping it",
                self.pending.len(),
                since.elapsed().as_secs_f32()
            );
            self.dead = true;
            self.pending.clear();
        }
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
                    // It is reading. Whatever it is still owed, it is not the
                    // connection `STUCK` exists to reap.
                    self.stalled_since = None;
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

        // READ only. Writability is asked about separately and only while
        // there is a backlog — see `Client::write_token` for what asking about
        // it unconditionally cost.
        let source = Generic::new(Shared(stream.clone()), Interest::READ, Mode::Level);
        let token = match self
            .loop_handle
            .insert_source(source, move |readiness, shared, state| {
                if readiness.readable {
                    state.ipc_read(id, &shared.0);
                }
                // Everything that run of messages left owing, before this
                // callback returns.
                //
                // This is what makes the deferral in `view_layout` invisible
                // rather than merely quick. calloop runs its sources one after
                // another and this one has just finished, so no other source —
                // no libinput event, no Wayland client, no vblank — can be
                // reached until it returns. Paying up here means nothing
                // outside this callback can ever observe a stack that is owed a
                // restack, whatever order calloop happens to run the rest in.
                state.settle();
                state.ipc.reap(&state.loop_handle.clone());
                Ok(PostAction::Continue)
            }) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!("could not register control client: {e}");
                return;
            }
        };

        // Before the stream is handed to the source, and best-effort: a
        // connection the kernel will not describe is an ordinary client, which
        // is what every connection was until there were two shells to tell
        // apart.
        //
        // Through rustix rather than `UnixStream::peer_cred`, which is still
        // unstable — and this is the same `SO_PEERCRED` either way.
        let pid = smithay::reexports::rustix::net::sockopt::socket_peercred(&*stream)
            .ok()
            .map(|cred| cred.pid.as_raw_nonzero().get());

        self.ipc.clients.insert(
            id,
            Client {
                stream,
                pid,
                framer: Framer::new(),
                pending: Vec::new(),
                stalled_since: None,
                token,
                write_token: None,
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
        if self.shells.is_empty() {
            return;
        }

        // By index, and every page: each engine has a mailbox of its own, so
        // which one a message came from is which one it was taken out of.
        for page in 0..self.shells.len() {
            // Before the messages: a page whose web process has died has
            // nothing further to say, and anything still queued was posted by
            // a document that no longer exists.
            if let Some(reason) = self.shells[page].engine.take_termination() {
                self.restart_shell(page, reason);
            }

            // Taken up front so nothing holds a borrow of `self` across the
            // dispatch, which needs it mutably.
            let messages = match self.shells.get(page) {
                Some(shell) => shell.engine.take_messages(),
                None => Vec::new(),
            };

            // Whose coordinates the page speaks in, said outright: an
            // in-process page has no connection and so no pid to find it by,
            // and deriving the origin from client id 0 found nothing at all —
            // every page's rectangles were taken as layout coordinates, which
            // is the very fault `dispatch_origin` exists to fix.
            let origin = match self.shells.get(page) {
                Some(shell) => shell.region.loc,
                None => continue,
            };

            for message in messages {
                tracing::debug!("from shell {page}: {message}");
                // Client id 0: the shell is not one of the socket clients, and
                // an error it caused goes to the broadcast channel it already
                // listens to rather than to a connection that does not exist.
                self.ipc_dispatch_at(0, origin, message.as_bytes());
            }
        }

        // A new frame is a reason to draw, and nothing else will ask: the
        // vblank loop stops when there is nothing left to submit, and the
        // nested backend draws only when it is asked to.
        self.needs_render = true;
        self.render_if_needed();
    }

    /// Parse one message and act on it.
    /// Which shell a control-socket client is, if it is one.
    ///
    /// By the pid the kernel reports for the connection, matched against the
    /// processes this compositor started. A client that merely says it is the
    /// desktop cannot be one: it does not choose its own pid.
    pub fn shell_for_client(&self, client_id: u64) -> Option<usize> {
        let pid = self.ipc.client_pid(client_id)?;
        self.shell_clients
            .iter()
            .position(|shell| shell.pid() == Some(pid))
    }

    pub fn ipc_dispatch(&mut self, client_id: u64, bytes: &[u8]) {
        // Whose coordinates these are, for a connection: the shell that holds
        // it, if it is one. Left at zero for everything else — a script driving
        // the socket speaks layout coordinates, because it has no page to speak
        // in.
        let origin = self
            .shell_for_client(client_id)
            .and_then(|at| self.shell_clients.get(at))
            .map(|shell| shell.region.loc)
            .unwrap_or_default();
        self.ipc_dispatch_at(client_id, origin, bytes);
    }

    /// The same, for a sender whose origin is known outright rather than
    /// through its connection — which is every in-process page, none of which
    /// has one.
    fn ipc_dispatch_at(
        &mut self,
        client_id: u64,
        origin: smithay::utils::Point<i32, smithay::utils::Logical>,
        bytes: &[u8],
    ) {
        // Everything that arrives, at debug. The out-of-process shell talks
        // over this socket like any other client, so without this there is no
        // way to see what the desktop asked for — which is the first question
        // whenever a click appears to do nothing.
        if tracing::enabled!(tracing::Level::DEBUG) {
            tracing::debug!("from {client_id}: {}", String::from_utf8_lossy(bytes));
        }

        // The first message the shell sends, once.
        //
        // "The shell did not lay anything out" has two very different causes:
        // the page never ran, or it ran and its layout was wrong. Nothing else
        // in the log distinguishes them.
        if !self.shell_announced {
            self.shell_announced = true;
            tracing::info!("shell is talking to us");
        }

        // Whose coordinates these are.
        //
        // A page lays its windows out in its own document, which starts at
        // (0, 0) however far across the desk the page itself begins — the DOM
        // has no idea it is on the second monitor. So a rectangle from a shell
        // is in that page's coordinates and has to be moved into the layout's
        // before anything is placed by it.
        //
        // What that cost, before this: with a `--url` page on the first screen
        // and the desktop on the second, a terminal opened on the second
        // monitor was drawn a frame there — the shell's own drawing is offset
        // correctly, being part of the page — and the window itself was mapped
        // at the same numbers taken as layout coordinates, which put it on the
        // *first* screen, on top of the page. A border with no window in it,
        // and a window where nothing asked for one.
        //
        // Left at zero for everything else: a script driving the socket speaks
        // layout coordinates, because it has no page to speak in.
        self.dispatch_origin = origin;

        match viewport_ipc::parse(bytes) {
            Ok(request) => self.handle_request(request),
            Err(error) => {
                tracing::debug!("rejected IPC message: {error}");
                self.ipc_reject(client_id, &error);
            }
        }

        // And back to zero, because it describes this dispatch and nothing
        // else. Left set, it was added a second time by anything that applies a
        // layout of its own afterwards: the recovery watchdog's rescue columns
        // are already in layout coordinates, and on a multi-monitor `--url`
        // session they landed a screen's width off the desk.
        self.dispatch_origin = (0, 0).into();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own, for the paths below to sit in.
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "viewport-ipc-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// The whole point: from the instant the socket exists at the name
    /// clients use, it is already closed to the world. There is no moment at
    /// which a wider mode could be observed, which is what bind-then-chmod
    /// could not say.
    #[test]
    fn a_socket_arrives_at_its_name_already_private() {
        let dir = scratch();
        let staging = staging_dir(&dir).expect("a staging directory");
        let final_path = dir.join("viewport-test.sock");
        let listener =
            listen_at(&staging.join("viewport-test.sock"), &final_path).expect("a socket");

        let mode = std::fs::metadata(&final_path)
            .expect("the socket at its well-known name")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the socket was ever connectable");

        drop(listener);
        let _ = std::fs::remove_file(&final_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The staging directory is owner-only and is not reused: a name that is
    /// taken — by whoever got there first — is an error to retry around, not
    /// a place to bind.
    #[test]
    fn the_staging_directory_is_private_and_fresh() {
        let dir = scratch();
        let first = staging_dir(&dir).expect("a staging directory");
        let mode = std::fs::metadata(&first)
            .expect("the directory")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        let second = staging_dir(&dir).expect("another staging directory");
        assert_ne!(first, second);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
