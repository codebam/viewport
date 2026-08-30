// SPDX-License-Identifier: GPL-3.0-or-later
//
// Applying a reloaded configuration to live compositor state.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// Apply a config file over the built-in defaults.
    ///
    /// Only what the file contains: a key left out never resets something a
    /// flag or an earlier load set, which is what makes a reload safe
    /// (`src/config.c:400`).
    pub fn apply_config(&mut self, file: crate::config::File) {
        if let Some(extensions) = file.layout_extensions {
            let mut extensions: Vec<_> = extensions
                .into_iter()
                .map(|(name, url)| viewport_ipc::event::LayoutExtension { name, url })
                .collect();
            extensions.sort_by(|a, b| a.name.cmp(&b.name));
            self.config.layout_extensions = extensions;
        }
        if let Some(layout) = file.layout {
            // Checked here for the same reason tiling_mode is, below: this is
            // where the name can be rejected while the file it came from is
            // still in hand. Unchecked, a typo reached the shell, matched none
            // of the models, and left it on the tiling default — while the
            // keymap was built for a layout that does not exist, so the chords
            // belonging to whichever one was meant were simply absent. What
            // that looks like is a config key that was ignored in silence.
            let extension = self
                .config
                .layout_extensions
                .iter()
                .any(|entry| entry.name == layout);
            if crate::config::BUILTIN_LAYOUTS.contains(&layout.as_str()) || extension {
                self.config.layout = layout;
            } else {
                tracing::warn!(
                    "unknown layout {layout:?}; expected a built-in ({}) or layout_extensions entry",
                    crate::config::BUILTIN_LAYOUTS.join(", ")
                );
                self.config.layout = "tiling".to_owned();
            }
        }
        if let Some(logo) = file.logo {
            self.config.logo = logo;
        }
        if let Some(crosses) = file.focus_crosses_outputs {
            self.config.focus_crosses_outputs = crosses;
        }
        if let Some(mode) = file.tiling_mode {
            // Checked here rather than in the shell, because this is where the
            // name can be rejected with the file it came from. An unknown one
            // would otherwise reach the shell, fail to match any arrangement,
            // and leave the tree manual with nothing said.
            const MODES: [&str; 5] = ["manual", "master-stack", "spiral", "bsp", "grid"];
            if MODES.contains(&mode.as_str()) {
                self.config.tiling_mode = Some(mode);
            } else {
                tracing::warn!(
                    "unknown tiling_mode {mode:?}; expected one of {}",
                    MODES.join(", ")
                );
            }
        }
        if let Some(workspaces) = file.workspaces {
            const MODES: [&str; 5] = ["manual", "master-stack", "spiral", "bsp", "grid"];
            let mut rules = Vec::with_capacity(workspaces.len());
            for (workspace, rule) in workspaces {
                if !(1..=9).contains(&workspace) {
                    tracing::warn!("workspace {workspace} is outside 1 through 9; ignoring it");
                    continue;
                }
                let extension = |layout: &str| {
                    self.config
                        .layout_extensions
                        .iter()
                        .any(|entry| entry.name == layout)
                };
                let layout = rule.layout.filter(|layout| {
                    let valid = crate::config::BUILTIN_LAYOUTS.contains(&layout.as_str())
                        || extension(layout);
                    if !valid {
                        tracing::warn!(
                            "workspace {workspace}: unknown layout {layout:?}; inheriting the global layout"
                        );
                    }
                    valid
                });
                let tiling_mode = rule.tiling_mode.filter(|mode| {
                    let valid = MODES.contains(&mode.as_str());
                    if !valid {
                        tracing::warn!(
                            "workspace {workspace}: unknown tiling_mode {mode:?}; inheriting the global mode"
                        );
                    }
                    valid
                });
                let mut gaps = viewport_ipc::event::Gaps {
                    inner: rule.gaps.inner,
                    outer: rule.gaps.outer,
                    smart: rule.gaps.smart,
                };
                if gaps.inner.is_some_and(|value| value < 0) {
                    tracing::warn!("workspace {workspace}: gaps.inner is negative; ignoring it");
                    gaps.inner = None;
                }
                if gaps.outer.is_some_and(|value| value < 0) {
                    tracing::warn!("workspace {workspace}: gaps.outer is negative; ignoring it");
                    gaps.outer = None;
                }
                let gaps = (gaps != viewport_ipc::event::Gaps::default()).then_some(gaps);
                rules.push(viewport_ipc::event::WorkspaceRule {
                    workspace,
                    output: rule.output.filter(|name| !name.trim().is_empty()),
                    layout,
                    tiling_mode,
                    gaps,
                });
            }
            rules.sort_by_key(|rule| rule.workspace);
            self.config.workspaces = rules;
        }
        if let Some(tutorial) = file.tutorial {
            self.config.tutorial = tutorial;
        }
        if let Some(bar) = file.bar {
            self.config.bar = Some(bar);
        }
        if file.rules.is_some() {
            self.config.rules = file.rules;
        }
        if file.theme.is_some() {
            self.config.theme = file.theme;
        }
        // The wallpaper, resolved here and not in the shell: the page is handed
        // a URL it can put straight in a `url()`, and a path that is not there
        // is said out loud at load rather than becoming a background-image that
        // quietly fails to fetch inside a web view nobody can open a console
        // on.
        //
        // A bad path is a warning and not a refusal to start. Every other key
        // in this file is a preference and this one is decoration; a session
        // that will not come up because a picture was moved is worse than a
        // session that comes up with the gradient it always had.
        if let Some(wallpaper) = file.wallpaper.as_deref() {
            // The empty string is how a file takes one away again, rather than
            // a null that a reload could not tell from an absent key.
            if wallpaper.trim().is_empty() {
                self.config.wallpaper = None;
            } else {
                match crate::config::wallpaper_value(wallpaper, "wallpaper") {
                    Ok(url) => self.config.wallpaper = Some(url),
                    Err(e) => tracing::warn!("{e}; keeping the current wallpaper"),
                }
            }
        }
        if let Some(mode) = file.wallpaper_mode.as_deref() {
            match crate::config::parse_wallpaper_mode(mode) {
                Ok(mode) => self.config.wallpaper_mode = Some(mode),
                Err(e) => tracing::warn!("{e}"),
            }
        }
        if file.gaps != crate::config::GapsConfig::default() {
            // Only fields the file actually names are forwarded; an absent one
            // leaves the shell's own default. A gap of zero is a deliberate
            // request (no spacing at all), so values are forwarded as-is
            // rather than skipped for being small.
            //
            // Negative is not a size. The runtime message that carries the
            // same setting refuses one (`config.gaps` in apply.rs), and a
            // reload that slipped one past here would forward it unchecked to
            // the shell — so the same refusal happens at the door, naming the
            // key, with that field keeping whatever it had.
            let prior = self.config.gaps.clone().unwrap_or_default();
            let mut gaps = viewport_ipc::event::Gaps {
                inner: file.gaps.inner,
                outer: file.gaps.outer,
                smart: file.gaps.smart,
            };
            if gaps.inner.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.gaps.inner {} is negative; keeping the current value",
                    gaps.inner.unwrap()
                );
                gaps.inner = prior.inner;
            }
            if gaps.outer.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.gaps.outer {} is negative; keeping the current value",
                    gaps.outer.unwrap()
                );
                gaps.outer = prior.outer;
            }
            self.config.gaps = Some(gaps);
        }
        if file.border != crate::config::BorderConfig::default() {
            // Checked for the same reason the gaps are: a negative radius or
            // width is refused by the runtime message, so it is refused here
            // too, and the field it named keeps what it had.
            let prior = self.config.border.clone().unwrap_or_default();
            let mut border = viewport_ipc::event::Border {
                radius: file.border.radius,
                width: file.border.width,
                smart: file.border.smart,
            };
            if border.radius.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.border.radius {} is negative; keeping the current value",
                    border.radius.unwrap()
                );
                border.radius = prior.radius;
            }
            if border.width.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.border.width {} is negative; keeping the current value",
                    border.width.unwrap()
                );
                border.width = prior.width;
            }
            self.config.border = Some(border);
        }
        // The clock's locale and format. Forwarded whole rather than field by
        // field, and only when the file names one of them: the shell's own
        // answer to an absent block is not a constant this side could write
        // down — it is whatever locale the engine is running under — so
        // sending a `clock` with three nulls in it would be the compositor
        // overruling that with nothing.
        if file.clock != crate::config::ClockConfig::default() {
            self.config.clock = Some(viewport_ipc::event::Clock {
                locale: file.clock.locale.clone(),
                hour12: file.clock.hour12,
                format: file.clock.format.clone(),
            });
        }
        // The bar. Two ways to ask for it: `bar_widgets` adds widgets to the
        // default module set; `bar_items` overrides the entire right side of
        // the bar with an explicit, ordered list of modules and widgets. When
        // `bar_items` is present (even empty) it wins outright — the shell
        // draws exactly what it lists and nothing else.
        //
        // The status sampler is told the same what-to-read as the shell lists:
        // which mounts to stat and whether to ask wpctl for the sink, so a bar
        // that draws neither spawns nothing.
        let bar_widgets: Vec<viewport_ipc::event::BarWidget> =
            file.bar_widgets.iter().map(bar_widget_ipc).collect();

        // The bar_items list, mapped to the IPC form. Bare strings are
        // modules; objects are widgets.
        let bar_items = file.bar_items.as_ref().map(|items| {
            items
                .iter()
                .map(|item| match item {
                    crate::config::BarItemConfig::Module(name) => {
                        viewport_ipc::event::BarItem::Module(name.clone())
                    }
                    crate::config::BarItemConfig::Widget(w) => {
                        viewport_ipc::event::BarItem::Widget(bar_widget_ipc(w))
                    }
                })
                .collect()
        });

        // Which of those actually get drawn: the override's own widgets when
        // present, else the bar_widgets additions. The sampler only pays for
        // what will be on screen.
        let drawn_widgets: Vec<&crate::config::BarWidgetConfig> =
            if let Some(items) = &file.bar_items {
                items
                    .iter()
                    .filter_map(|item| match item {
                        crate::config::BarItemConfig::Widget(w) => Some(w),
                        crate::config::BarItemConfig::Module(_) => None,
                    })
                    .collect()
            } else {
                file.bar_widgets.iter().collect()
            };

        self.config.bar_widgets = if file.bar_items.is_some() {
            // Superseded: the whole right side comes from bar_items, so the
            // shell is told the override and not the additions it replaces.
            None
        } else if bar_widgets.is_empty() {
            None
        } else {
            Some(bar_widgets)
        };
        self.config.bar_items = bar_items;
        // One fold over what is drawn, and every sampler knows its job.
        let mut sampled = Sampling::default();
        for widget in &drawn_widgets {
            let costs = widget.sampling();
            sampled.mounts.extend(costs.mounts);
            sampled.volume |= costs.volume;
            sampled.mic |= costs.mic;
            sampled.players |= costs.players;
            sampled.battery |= costs.battery;
        }
        self.status
            .configure(sampled.mounts, sampled.volume, sampled.mic);
        // Following every media player on the session is worth doing only for
        // a bar that draws one, which is the same rule the audio sampling
        // above follows. The battery likewise, on the power worker's own
        // switch.
        self.mpris.set_enabled(sampled.players);
        self.power.set_widget(sampled.battery);
        self.ai_usage.configure(
            drawn_widgets
                .iter()
                .filter_map(|widget| crate::ai_usage::account(widget))
                .collect(),
        );
        if let Some(url) = file.url {
            self.shell_url = Some(url);
        }
        if let Some(span) = file.url_span {
            self.shell_url_spans = span;
        }
        // Only where the command line said nothing: a flag is a decision made
        // for this run, and a config file that could override it would make
        // `--shell-backend` untestable on a machine that has one.
        if let Some(name) = file.shell_backend.as_deref() {
            if !self.shell_backend_from_flag {
                self.shell_backend = crate::shell_backend::choose(None, Some(name));
            }
        }
        if !file.outputs.is_empty() {
            let stale_mirrors: Vec<String> = self
                .output_config
                .iter()
                .filter(|(name, previous)| {
                    previous.mirror.is_some()
                        && file.outputs.get(*name).is_some_and(|next| next.mirror.is_none())
                })
                .map(|(name, _)| name.clone())
                .collect();
            let stale_vrr: Vec<String> = self
                .output_config
                .iter()
                .filter(|(name, previous)| {
                    previous.vrr.is_some()
                        && file.outputs.get(*name).is_some_and(|next| next.vrr.is_none())
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in stale_mirrors {
                if let Some(output) = self.any_output_by_name(&name) {
                    let _ = self.configure_mirror(&output, None);
                }
            }
            for name in stale_vrr {
                self.output_vrr.remove(&name);
                self.output_vrr_wanted.remove(&name);
            }
            self.output_config = file.outputs;
            // Carried out here too, and not left for the next hotplug: the
            // block is otherwise applied only where an output arrives, and a
            // reload has no arrival to borrow. On the first load the outputs
            // do not exist yet, so this walks the block and keeps it — which
            // is what the comment on `apply_output_config` describes — and on
            // a reload it is what makes the file the last word again.
            self.apply_output_config();
        }
        if let Some(input) = file.input {
            self.input_config = input;
            self.apply_libinput_config();
        }
        if let Some(gestures) = file.gestures {
            let mut specs: Vec<_> = gestures.into_iter().collect();
            specs.sort_by(|a, b| a.0.cmp(&b.0));
            self.gestures = specs
                .into_iter()
                .filter_map(|(gesture, action)| {
                    match crate::input::parse_gesture(&gesture, &action) {
                        Some(binding) => Some(binding),
                        None => {
                            tracing::warn!("invalid gesture {gesture:?}");
                            None
                        }
                    }
                })
                .collect();
            // Keep any captured sequence captured. Forwarding its update or
            // end after consuming its begin would give a client half a gesture.
        }
        // Run after the compositor is up, so it reaches whatever it names.
        if let Some(command) = file.startup.as_deref() {
            self.startup = Some(command.to_owned());
        }
        if let Some(url) = file.fallback {
            self.fallback_url = Some(url);
        }
        if let Some(ms) = file.timeout_ms {
            self.load_timeout_ms = ms.max(0) as u64;
        }
        if let Some(allowed) = file.vt_switching {
            self.vt_switching = allowed;
        }
        if let Some(mode) = file.osk.as_deref() {
            match crate::config::parse_osk_mode(mode) {
                Ok(mode) => {
                    self.osk_mode = mode;
                    self.config.osk = mode.as_str().to_owned();
                    // A reload can turn the keyboard off, or take a touch
                    // desk from manual back to auto, while a client's
                    // text-input is still enabled — recomputing right away is
                    // what makes that take effect immediately rather than at
                    // the next commit or focus change. Only half the story:
                    // this can lower a keyboard `osk.wanted` raised, but not
                    // one somebody pinned open by hand with the chord, which
                    // is not this function's to know about. That half is the
                    // shell's, driven by the `osk` field notify_config sends
                    // right after this — see `applyOskMode` in `osk.js`.
                    self.sync_osk_wanted();
                }
                Err(e) => tracing::warn!("{e}; leaving osk as {:?}", self.osk_mode.as_str()),
            }
        }
        if let Some(setting) = file.xwayland.scale.as_ref() {
            match crate::config::parse_xwayland_scale(setting) {
                Ok(scale) => {
                    // Recorded, and acted on only by `start_xwayland`. A
                    // reload that changes this says nothing here on purpose:
                    // the log line belongs where the number is used, and
                    // there is nothing this function could do with a new one
                    // — the X screen's size is fixed when the server starts.
                    self.xwayland_scale = scale;
                }
                Err(e) => tracing::warn!(
                    "{e}; leaving the xwayland scale at {}",
                    self.xwayland_scale.as_str()
                ),
            }
        }
        if let Some(dark) = file.dark_mode {
            self.dark_mode = dark;
            // Running applications change on the portal's signal; without this
            // a reload would move the setting and nothing on screen with it.
            self.appearance.set_dark(dark);
            // And the shell, which draws the switch: the config event carries
            // the scheme so a settings panel can show what it is rather than
            // guess. Set here as well as at startup because a reload is the
            // other way the value moves without anybody pressing the chord.
            self.config.dark_mode = dark;
        }
        if let Some(vrr) = file.adaptive_sync {
            self.adaptive_sync = vrr;
            self.output_vrr_wanted.clear();
            self.needs_render = true;
        }
        if let Some(mode) = file.decorations.as_deref() {
            // "client" hands the frame back; anything else, including a value
            // nobody recognises, keeps it here (`src/config.c:315`).
            self.server_decorations = mode != "client";
            // Keep the KDE manager's advertised default in step with the
            // per-surface answer in handlers/xdg_shell.rs::decoration_mode:
            // a client that probes the manager to decide whether to draw a
            // frame and a client that asks per-surface must get the same
            // answer, or one draws nothing while the other also draws nothing.
            use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
            self.kde_decoration_state
                .set_default_mode(if self.server_decorations {
                    KdeDefaultMode::Server
                } else {
                    KdeDefaultMode::Client
                });
        }
        // What a notification with no sound hints of its own plays. Set on
        // every load rather than only when it changed, and unconditionally
        // rather than behind a `!= default()` guard: absence here means
        // silence, so a reload that removed the key has to reach the server
        // thread as `None` or the old sound outlives the configuration that
        // asked for it.
        self.notifications
            .set_default_sound(crate::sound::Sound::from_config(
                file.notifications.sound_file.as_deref(),
                file.notifications.sound_name.as_deref(),
            ));

        // How much of a record to keep, on the same terms: applied on every
        // load, so a reload that lowers it drops the oldest entries there and
        // then, and one that sets zero empties the centre rather than leaving
        // what is already in it behind a setting that says it keeps nothing.
        let before = self.notification_history.entries().len();
        self.notification_history.set_limit(
            file.notifications
                .history
                .unwrap_or(crate::notification::DEFAULT_HISTORY),
        );
        if self.notification_history.entries().len() != before {
            self.publish_notification_history();
        }

        // The tray, on unless the file turns it off. Applied on every load, so
        // a reload that flips it claims or releases the bus names then and
        // there rather than at the next restart — the same property the
        // stylesheet and the keybindings have.
        self.tray_enabled = file.tray.unwrap_or(true);
        self.tray.set_enabled(self.tray_enabled);
        // How much the clipboard remembers, or nothing at all. Applied on
        // every load, so a reload that turns it off empties it there and then
        // rather than at the next restart.
        self.clipboard
            .set_limit(file.clipboard_history.unwrap_or(25));

        self.tray.set_icon_theme(
            file.icon_theme
                .clone()
                .unwrap_or_else(|| "hicolor".to_owned()),
        );
        // The same key, for the icons the launcher resolves. The tray keeps
        // its own copy for its worker; this one travels with every query sent
        // to the launcher's scanner.
        self.icon_theme = file
            .icon_theme
            .clone()
            .unwrap_or_else(|| "hicolor".to_owned());
        // Applied on every load, so a reload that changes the theme empties
        // the cache then and there rather than at the next restart — the same
        // property the tray cache has, and the thing the user reaches for when
        // they install icons and want to see them. The cache is the scanner's
        // now, so the emptying is a message rather than a method.
        self.launcher_scan.clear_icons();

        if file.idle != crate::config::IdleConfig::default() {
            self.idle_settings = crate::idle::Settings {
                lock_after: file.idle.lock_after,
                blank_after: file.idle.blank_after,
                lock_command: file.idle.lock_command,
            };
        }

        // The one answer to what locking means, worked out once per config
        // load rather than at each lock. Every path that locks — the idle
        // deadline, the `lock` binding, the lid, the power menu's Lock row —
        // goes through `lock_session`, which reads this and nothing else.
        self.lock_mode =
            crate::lock::Mode::from_command(self.idle_settings.lock_command.as_deref());

        self.lid = match file.lid.as_deref() {
            Some(name) => match crate::power::LidAction::parse(name) {
                Some(action) => action,
                None => {
                    tracing::warn!(
                        "lid: {name:?} is not ignore, lock, blank or suspend; leaving as {:?}",
                        self.lid
                    );
                    self.lid
                }
            },
            None => crate::power::LidAction::default_for(self.idle_settings.lock_command.is_some()),
        };
        self.power
            .set_enabled(self.power.widget() || self.lid != crate::power::LidAction::Ignore);

        // The cursor theme, resolved against what is already loaded rather
        // than round-tripped through the process environment. Writing environ
        // on a process whose tray, status and notification threads are live
        // and reading it is undefined — glibc may free or rehash it under a
        // concurrent getenv — and every reload used to do exactly that, twice,
        // whether anything had changed or not.
        let theme = file
            .cursor
            .theme
            .clone()
            .unwrap_or_else(|| self.cursor_theme.name().to_owned());
        let size = file.cursor.size.unwrap_or(self.cursor_theme.size());
        // Only when one of those two moved, and only then: rebuilding on any
        // change to the block would throw the loaded images away because a
        // reload touched the hide deadline, which has nothing to do with what
        // they look like.
        if theme != self.cursor_theme.name() || size != self.cursor_theme.size() {
            // Built straight from the pair above: `Theme::named` takes the
            // values itself, so the loader never needs environ as a go-between.
            // There was a time this wrote `XCURSOR_THEME` and `XCURSOR_SIZE`
            // into the process environment for the old constructor to read
            // straight back — a setenv on a live process, undefined against
            // every thread that might be mid-getenv, run on every reload that
            // touched the cursor block.
            self.cursor_theme = crate::cursor::Theme::named(theme, size);
            // And what the portal answers, or a toolkit keeps sizing its own
            // cursors from the value it was told when it started — which is a
            // pointer that changes size as it crosses into a window, and a
            // setting that appears not to have been respected at all.
            self.appearance
                .set_cursor(self.cursor_theme.name(), self.cursor_theme.size() as i32);
            // The pointer on screen is still the old image, and the compositor
            // draws on damage: nothing else here is damage.
            self.needs_render = true;
        }
        // The magnifier's step and its ceiling. A reload that lowers the
        // ceiling below where the screen is brings the picture back down to
        // meet it, which is a repaint nothing else here would ask for.
        if self
            .magnifier
            .configure(file.magnify.step, file.magnify.max)
        {
            self.needs_render = true;
        }
        if self.cursor_hide.set_after_ms(file.cursor.hide_after_ms) {
            // The deadline that hid it has just been taken away, so nothing
            // else would ever bring it back.
            self.needs_render = true;
        }
        // A file that turned it on gets a countdown without waiting for the
        // pointer to move first, which for a desk nobody is at is never.
        self.arm_cursor_hide();

        // The keymap, if the file names one. Replacing the keyboard is how
        // this is set — there is no way to change the layout of one that
        // already exists — so it happens before any client has seen a seat.
        let keyboard = &file.keyboard;
        if keyboard != &crate::config::KeyboardConfig::default() {
            let xkb = smithay::input::keyboard::XkbConfig {
                layout: keyboard.layout.as_deref().unwrap_or(""),
                variant: keyboard.variant.as_deref().unwrap_or(""),
                options: keyboard.options.clone(),
                ..Default::default()
            };
            // C's defaults, which are sway's (`src/main.c`): 25 a second after
            // 200ms.
            //
            // Zero and below are refused rather than handed to
            // `seat.add_keyboard`: a rate of zero is a key that repeats never
            // and a delay of zero is one that never stops, and the runtime
            // message that sets these is checked the same way. The field keeps
            // the default the refused value would have displaced.
            let delay = match keyboard.repeat_delay {
                Some(delay) if delay <= 0 => {
                    tracing::warn!(
                        "keyboard.repeat_delay {delay} is not positive; keeping the default 200"
                    );
                    200
                }
                Some(delay) => delay,
                None => 200,
            };
            let rate = match keyboard.repeat_rate {
                Some(rate) if rate <= 0 => {
                    tracing::warn!(
                        "keyboard.repeat_rate {rate} is not positive; keeping the default 25"
                    );
                    25
                }
                Some(rate) => rate,
                None => 25,
            };
            match self.seat.add_keyboard(xkb, delay, rate) {
                Ok(_) => {
                    // Written down for anything that has to be told what this
                    // desk types in rather than asked to guess — which today
                    // is a libei client, whose keymap is sent to it when its
                    // keyboard device is made. Only on success: a layout that
                    // was refused left the previous keymap in place, and
                    // recording the refused one would send a remote client a
                    // keymap the seat is not using.
                    self.keyboard_config = keyboard.clone();
                    tracing::info!(
                        "keymap {:?}{}, repeat {rate}/s after {delay}ms",
                        keyboard.layout.as_deref().unwrap_or("(default)"),
                        keyboard
                            .variant
                            .as_deref()
                            .map(|v| format!(" {v}"))
                            .unwrap_or_default(),
                    );
                }
                // Naming it matters: an unknown layout otherwise leaves the
                // built-in one in place and looks like the config was ignored.
                Err(e) => tracing::error!(
                    "keymap {:?} was refused, keeping the current one: {e}",
                    keyboard.layout.as_deref().unwrap_or("(default)")
                ),
            }
        }

        // Bindings last, because whether the defaults are there at all depends
        // on the file. Presence of "binds" means "this is the whole keymap",
        // so an empty one asks for none.
        let terminal = file
            .terminal
            .or_else(|| std::env::var("VIEWPORT_TERMINAL").ok())
            .unwrap_or_else(|| "foot".to_owned());
        // The external menu command, when one is named. Absent — the key left
        // out and the variable unset — is the built-in launcher, which is
        // what `Mod4+d` opens by default now that the shell draws one.
        let menu = file.menu.or_else(|| std::env::var("VIEWPORT_MENU").ok());
        let layout = self.config.layout.clone();

        // Which terminal the wallpaper is, resolved against the same
        // `terminal` the keymap uses so `true` means the one Mod4+Return
        // already opens.
        //
        // Only when the file says something: a reload that leaves the key out
        // must not take down a wallpaper a flag asked for, which is the rule
        // every other key here follows. Nothing is started or stopped from
        // here either — `start_background_process` does that once the outputs
        // exist, and a config reload cannot spawn a process behind the
        // desktop.
        self.terminal = terminal.clone();
        if file.background_terminal.is_some() {
            self.background_command =
                crate::background::resolve(file.background_terminal.as_ref(), &terminal);
            self.config.background_terminal = self.background_command.is_some();
        }

        let mut bindings = Vec::new();
        // Overrides go in front: bindings are matched first-wins, so a chord
        // the file claims shadows the default without the default needing to
        // be removed.
        if let Some(over) = file.binds_override.as_ref() {
            bindings.extend(
                crate::config::bind_specs(over)
                    .iter()
                    .filter_map(|spec| crate::binding::parse(spec)),
            );
        }
        match file.binds.as_ref() {
            Some(binds) => bindings.extend(
                crate::config::bind_specs(binds)
                    .iter()
                    .filter_map(|spec| crate::binding::parse(spec)),
            ),
            None => bindings.extend(crate::binding::defaults(
                &terminal,
                menu.as_deref(),
                &layout,
            )),
        }
        crate::binding::guarantee_an_exit(&mut bindings);
        self.bindings = bindings;
    }

    /// How many empty ticks before the barrier clock stops. A second at sixty
    /// hertz, and a commit starts it again.
    const QUIET: u32 = 60;
}
