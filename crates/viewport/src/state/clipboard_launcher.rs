// SPDX-License-Identifier: GPL-3.0-or-later
//
// Clipboard history and launcher query/launch handling.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// Read whatever is on the clipboard now, for the history.
    ///
    /// Called from an idle rather than from the selection handler, because
    /// smithay runs that handler before it stores the new selection: asking
    /// the seat inside it hands back the *previous* client's data. See
    /// `SelectionHandler::new_selection` in `handlers`.
    pub fn capture_clipboard(&mut self, mime: String) {
        use smithay::wayland::selection::data_device::request_data_device_client_selection;

        if !self.clipboard.enabled() {
            return;
        }
        // A pipe: the client fills the write end, a thread reads this one.
        // Both ends run on another process's schedule, which is why neither is
        // touched on the compositor's thread.
        let (read, write) = match smithay::reexports::rustix::pipe::pipe() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("no pipe for the clipboard: {e}");
                return;
            }
        };
        if let Err(e) = request_data_device_client_selection(&self.seat, mime, write) {
            // The client that owned it has gone in the meantime, or offered a
            // type it will not actually send. Neither is worth more than a
            // debug line: the next copy is another chance.
            tracing::debug!("could not ask for the selection: {e}");
            return;
        }
        self.clipboard.capture(read);
    }

    /// Tell the shell what the clipboard history holds.
    ///
    /// Sent whole, whenever it changes and whenever a picker asks: it is a
    /// short list drawn in one pass, and a shell that reconciled adds against
    /// removes would be doing bookkeeping to save a message sent when somebody
    /// presses copy.
    pub fn notify_clipboard(&mut self) {
        let entries = self.clipboard.entries().to_vec();
        self.notify(&viewport_ipc::Event::ClipboardHistory { entries });
    }

    /// Offer the entry the history has just moved to the top, with the
    /// compositor as the selection's owner.
    ///
    /// This is the whole point of keeping a history: the application that
    /// copied something may have exited hours ago, and a Wayland selection
    /// lives only as long as the client offering it. Owning it here means the
    /// compositor answers when a client pastes — see `send_selection` in
    /// `handlers`.
    pub fn offer_clipboard(&mut self) {
        use smithay::wayland::selection::data_device::set_data_device_selection;
        let dh = self.display_handle.clone();
        set_data_device_selection(
            &dh,
            &self.seat,
            crate::clipboard::offered_mimes(),
            crate::clipboard::Owner::History,
        );
    }

    /// The largest list the launcher answers with.
    ///
    /// A row is a name and an icon, and a desktop with four hundred
    /// applications is not a list somebody scrolls — it is a filter waiting to
    /// be typed.
    const LAUNCHER_LIMIT: usize = 96;

    /// The applications the launcher can start, filtered, as the shell draws
    /// them.
    ///
    /// Scanned off this thread and cached briefly: keystrokes filter one
    /// snapshot, while a package installed during the session appears on the
    /// next refresh after the short cache deadline. What happens here is the
    /// posting of the question; the answer comes back through the loop like
    /// any other message, to `launcher_apply`. The filter is the shell's text, matched
    /// case-insensitively against the name, what the entry says it is for,
    /// and the command it runs — a binary name typed into the field finds its
    /// entry — and against the app_id a token is minted under; absent is the
    /// whole list.
    ///
    /// The query answers with a generation, the number of queries the
    /// compositor has answered, and `launcher.launch` carries it back: a
    /// launch naming a generation the compositor has moved past is a row from
    /// a list the query that replaced it has not answered yet.
    pub fn launcher_query(&mut self, filter: Option<String>) {
        self.launcher_generation += 1;
        let query = crate::launcher::Query {
            generation: self.launcher_generation,
            filter,
            theme: self.icon_theme.clone(),
            limit: Self::LAUNCHER_LIMIT,
        };
        if self.launcher_scan.online() {
            self.launcher_scan.ask(query);
        } else {
            // No thread to ask. Answered here, then: the blocking path this
            // used to be always, kept for the session where the thread would
            // not start. Its icon resolutions are thrown away when the query
            // ends, which a working scanner never throws away — but a session
            // without the thread is already the degraded one.
            let dirs = crate::launcher::directories();
            let desktop = crate::launcher::current_desktop();
            let desktop: Vec<&str> = desktop.iter().map(String::as_str).collect();
            let mut icons = std::collections::HashMap::new();
            let answer = crate::launcher::answer(&query, &dirs, &desktop, &mut icons);
            self.launcher_apply(answer);
        }
    }

    /// Apply a finished scan: the list a launch will be naming into, and the
    /// rows the shell draws.
    ///
    /// Arrives on the loop from the scanner thread. An answer older than the
    /// newest query is dropped rather than drawn — the keystrokes kept coming
    /// while it was being built, and the shell wants the last word, not the
    /// first. They are answered in order, so this only ever passes over a
    /// list a later query has already superseded.
    pub fn launcher_apply(&mut self, answer: crate::launcher::Answer) {
        if answer.generation != self.launcher_generation {
            return;
        }
        // What `launcher.launch` will be naming an index into.
        self.launcher_list = answer.rows.iter().map(|row| row.app.clone()).collect();
        let apps = answer
            .rows
            .iter()
            .enumerate()
            .map(|(id, row)| viewport_ipc::event::LauncherApp {
                id: id as u32,
                name: row.app.name.clone(),
                icon: row.icon.clone(),
                detail: row.app.detail.clone(),
            })
            .collect();
        self.notify(&viewport_ipc::Event::LauncherList {
            generation: self.launcher_generation,
            apps,
        });
    }

    /// Start the application the picker's highlighted row named.
    ///
    /// The process is handed an xdg-activation token minted for it, so the
    /// window that appears opens focused rather than behind whatever the user
    /// moved on to — the launcher knows where the window is going, because it
    /// is the thing that asked for it, and the token is how it says so.
    ///
    /// `generation` is the list the row came from. The picker sends a query
    /// on every keystroke and does not wait for the answer before it lets the
    /// user press Enter, so the list a row is drawn from may already have
    /// been replaced by the time the launch lands: an `id` from the old list
    /// is almost always in range of the new one, and that is how the wrong
    /// application starts. A launch that names a generation the compositor
    /// has moved past is refused.
    pub fn launcher_launch(&mut self, id: u32, generation: u64) {
        if generation != self.launcher_generation {
            self.notify(&viewport_ipc::Event::Error {
                context: "launcher.launch".to_owned(),
                message: format!("the list {generation} is no longer the one on screen"),
            });
            return;
        }
        let Some(app) = self.launcher_list.get(id as usize).cloned() else {
            // An `id` from a list the next query replaced. The picker is
            // closing either way; the error is for the log and for a script.
            self.notify(&viewport_ipc::Event::Error {
                context: "launcher.launch".to_owned(),
                message: format!("no such application {id}"),
            });
            return;
        };

        // A token nobody presented in a minute is not one an application is
        // still coming back with. Pruned here, on the way out, rather than on
        // a timer the event loop would have to run.
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < std::time::Duration::from_secs(60));
        let (token, _) = self.xdg_activation_state.create_external_token(Some(
            smithay::wayland::xdg_activation::XdgActivationTokenData {
                app_id: Some(app.app_id.clone()),
                ..Default::default()
            },
        ));
        let token = token.as_str().to_owned();

        // `Terminal=true` is run in the terminal `Mod4+Return` opens, the way
        // an external menu does it: the entry names the program, the session
        // names the window it runs in. The terminal is the session's command
        // line, bare — it may be more than one word, and a quote is what
        // makes the shell look for a binary of the whole line's literal name.
        let command = if app.terminal {
            format!("{} -e {}", self.terminal, app.exec)
        } else {
            app.exec
        };
        // The cursor pair goes with it, as this session draws it now rather
        // than as the environment said when the compositor started: a reload
        // that changed the theme no longer writes environ behind the worker
        // threads' backs, so the child is told here instead of inheriting.
        // DISPLAY with it, for the same reason: Xwayland reports ready long
        // after this process started, and environ is not written then either.
        let mut extra = vec![
            ("XDG_ACTIVATION_TOKEN".to_owned(), token),
            (
                "XCURSOR_THEME".to_owned(),
                self.cursor_theme.name().to_owned(),
            ),
            (
                "XCURSOR_SIZE".to_owned(),
                self.cursor_theme.size().to_string(),
            ),
        ];
        extra.extend(self.child_display_env());
        crate::input::spawn_with_env(&command, &extra);
    }
}
