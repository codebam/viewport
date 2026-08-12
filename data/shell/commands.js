/* SPDX-License-Identifier: MIT
 *
 * Commands from the compositor, and the inbound message loop.
 *
 * Loaded last: the bottom of this file asks the compositor for the state the
 * shell starts from, and everything that handles the answer has to exist by
 * then.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
/* ------------------------------------------------------------------------
 * Commands forwarded from the compositor
 * --------------------------------------------------------------------- */

function handleShellCommand(command, args) {
  const arg = args[0];
  const n = Number(arg);

  switch (command) {
    case 'workspace.switch':
      clearSelection();
      if (Number.isFinite(n)) switchWorkspace(activeOutputName(), n);
      break;
    case 'workspace.back':
      clearSelection();
      workspaceBack(activeOutputName());
      break;
    case 'workspace.next':
      clearSelection();
      stepWorkspace(activeOutputName(), 1);
      break;
    case 'workspace.prev':
      clearSelection();
      stepWorkspace(activeOutputName(), -1);
      break;
    case 'workspace.move':
      if (Number.isFinite(n)) moveToWorkspace(n);
      break;
    case 'window.fullscreen':
      toggleFullscreen();
      break;
    case 'window.focus_parent':
      focusParent();
      break;
    case 'window.move': {
      if (focusedId == null && selectedIds.size === 0) break;

      /* On the canvas a window is moved across the plane rather than through a
         tree, and running off the edge of the screen is not running out of
         workspace — the plane has no edge, and the view follows. Falling
         through to moveViewToOutput would carry the window to the other
         monitor the first time it reached the edge of this one, which is a
         plane that is not infinite after all. */
      if (layoutMode === 'canvas') {
        if (focusedId != null) canvasMoveFocused(arg);
        break;
      }

      if (layoutMode === 'scrolling') {
        if (selectedIds.size > 0) {
          if (!scrollMoveSelected(arg) && focusedId != null) {
            moveViewToOutput(focusedId, arg);
          }
        } else if (focusedId != null) {
          if (!scrollMove(arg, focusedId)) {
            moveViewToOutput(focusedId, arg);
          }
        }
        break;
      }

      if (selectedContainer != null && selectedContainer !== workspaces.get(activeWorkspace())) {
        if (!moveContainer(selectedContainer, arg) && focusedId != null) {
          moveViewToOutput(focusedId, arg);
        }
      } else if (focusedId != null) {
        if (isFloating(focusedId)) {
          const step = 40;
          moveByDelta(focusedId,
            arg === 'left' ? -step : arg === 'right' ? step : 0,
            arg === 'up' ? -step : arg === 'down' ? step : 0);
        } else if (!moveLeaf(focusedId, arg)) {
          moveViewToOutput(focusedId, arg);
        } else {
          relayoutAll();
        }
      }
      break;
    }
    case 'layout.split':
      pendingSplit = arg === 'vertical' ? 'vertical' : 'horizontal';
      break;
    /* Switch arrangement without editing the config file. No argument cycles,
       which is what a single key wants to do; a name picks one outright. */
    case 'layout.mode': {
      const next = TILING_MODES.includes(arg)
        ? arg
        : TILING_MODES[(TILING_MODES.indexOf(tilingMode) + 1) % TILING_MODES.length];
      if (next !== tilingMode) {
        tilingMode = next;
        /* Nothing to invalidate: the next relayout works out what the shape
           should be and compares it against what is there. */
        relayoutAll();
      }
      break;
    }
    /* Switch layout model without editing the config file. No argument cycles,
       which is what a single key wants to do; a name picks one outright. The
       tree survives the switch — every model reads the same one — so this is a
       change of presentation and not of what is open. */
    case 'layout.model': {
      const next = LAYOUT_MODES.includes(arg)
        ? arg
        : LAYOUT_MODES[(LAYOUT_MODES.indexOf(layoutMode) + 1) % LAYOUT_MODES.length];
      if (next !== layoutMode) {
        layoutMode = next;
        clearSelection();
        normaliseForLayout();
        relayoutAll();
      }
      break;
    }
    case 'bar.toggle':
      toggleBar();
      break;
    case 'layout.toggle':
      toggleLayout();
      break;
    case 'layout.resize':
      resizeFocused(arg);
      break;
    case 'window.fullscreen.set': {
      /* A client asked to go fullscreen itself, e.g. a video player. */
      const id = Number(args[0]);
      const on = args[1] === '1';
      if (Number.isFinite(id)) {
        /* The client asked for this itself, so it already knows — just lay it
         * out, without echoing the state back and starting a loop. */
        const workspace = workspaceOf(id);
        if (workspace !== null) {
          if (on) {
            fullscreens.set(workspace, id);
          } else if (fullscreens.get(workspace) === id) {
            fullscreens.delete(workspace);
          }
        }
        relayoutAll();
      }
      break;
    }
    case 'layout.resize.delta':
      resizeByDelta(Number(args[0]), Number(args[1]), Number(args[2]));
      break;
    case 'layout.move.delta':
      moveByDelta(Number(args[0]), Number(args[1]), Number(args[2]));
      break;
    /* Mod4 + left drag on the desktop, where there is no window to take hold
       of. Only the canvas has a view to move, so it is a no-op everywhere else
       — the compositor cannot tell which layout is running and should not have
       to guess. */
    case 'canvas.pan.delta':
      canvasPanBy(Number(args[0]), Number(args[1]));
      break;
    case 'layout.float.toggle':
      toggleFloating(focusedId);
      break;
    case 'layout.tabbed':
      setContainerLayout('tabbed');
      break;
    case 'layout.stacked':
      setContainerLayout('stacked');
      break;

    /* Scrolling layout. Bound only when the compositor is configured for it,
       but harmless to receive otherwise. */
    case 'layout.focus':
      clearSelection();
      scrollFocus(arg);
      break;
    case 'layout.consume':
      consumeWindow();
      break;
    case 'layout.expel':
      expelWindow();
      break;
    case 'layout.column.width':
      cycleColumnWidth();
      break;
    case 'layout.column.height':
      cycleWindowHeight();
      break;

    /* Touchpad. The compositor keeps three-finger swipes for itself and sends
       them here; everything else goes to the focused client. */
    case 'gesture.scroll':
      gestureScroll(Number(arg));
      break;
    case 'gesture.settle':
      gestureSettle();
      break;
    case 'layout.overview':
      setOverview(!overviewActive);
      break;

    /* Solar. Bound only when the compositor is configured for it, and each of
       these is a no-op in the other two layouts rather than an error: a chord
       left over in someone's config file should do nothing, not log. */
    case 'solar.ray':
      if (layoutMode === 'solar') {
        clearSelection();
        solarRay(arg);
      }
      break;
    case 'solar.spin':
      if (layoutMode === 'solar') solarSpin(Number(arg) < 0 ? -1 : 1);
      break;
    case 'solar.slingshot':
      if (layoutMode === 'solar') solarSlingshot();
      break;
    case 'solar.mass':
      if (layoutMode === 'solar') solarMass(Number(arg) < 0 ? -1 : 1);
      break;
    case 'solar.field':
      if (layoutMode === 'solar') solarToggleField();
      break;

    /* Canvas. Bound only when the compositor is configured for it, and each of
       these is a no-op in the other layouts rather than an error — the same
       bargain solar's chords make. canvasTarget() is where that no-op lives,
       so nothing here has to ask twice. */
    case 'canvas.pan':
      canvasPanDirection(arg);
      break;
    case 'canvas.zoom':
      canvasZoom(arg);
      break;
    case 'canvas.fit':
      canvasFit();
      break;
    case 'canvas.home':
      canvasHome();
      break;
    case 'output.hdr':
      /* No state of its own: the compositor owns whether an output is in HDR,
         and toggling is asking it to flip whatever it currently has. */
      send({ type: 'output.hdr', name: activeOutputName() });
      break;
    case 'workspace.step':
      clearSelection();
      stepWorkspace(Number(arg));
      break;
    case 'mode.changed':
      currentMode = arg || 'default';
      renderBars();
      break;
    case 'output.focus':
      focusOutputDirection(arg);
      break;
    default:
      console.warn('unknown shell command:', command, args);
  }
}

/* ------------------------------------------------------------------------
 * Inbound
 * --------------------------------------------------------------------- */

window.addEventListener('viewport', (event) => {
  const message = event.detail;

  switch (message.type) {
    case 'config':
      /* Which layout model to run. Sent on connect and on reload, so switching
         it in the config file and reloading takes effect without a restart —
         the tree survives, it is only presented differently. */
      windowRules = Array.isArray(message.rules) ? message.rules : [];
      applyTheme(message.theme);
      applyGaps(message.gaps);
      applyBorder(message.border);
      applyBarMode(message.bar);
      applyBarWidgets(message.bar_widgets);
      applyBarItems(message.bar_items);
      /* Absent means on: a config file that says nothing should get the
         explanation, and only someone who has read it once turns it off. */
      document.documentElement.classList.toggle('no-logo',
        message.logo === false);
      document.documentElement.classList.toggle('no-tutorial',
        message.tutorial === false);
      /* Something is being drawn behind the page — a terminal, as the
         wallpaper — so the gradient in `body` has to go, or it is painted over
         the thing it is meant to reveal. Absent means nothing is back there,
         which is every desktop that has not asked for one. */
      document.documentElement.classList.toggle('behind',
        message.background_terminal === true);
      /* Absent means on, matching the compositor's own default: only an
         explicit false keeps focus on the monitor it is on. */
      focusCrossesOutputs = message.focus_crosses_outputs !== false;
      /* Absent is manual: the tree of splits this has always built. A mode
         arriving while windows are open rearranges them on the next relayout,
         so switching it in the config file and reloading is enough. */
      {
        const next = TILING_MODES.includes(message.tiling_mode)
          ? message.tiling_mode : 'manual';
        if (next !== tilingMode) {
          tilingMode = next;
          relayoutAll();
        }
      }
      if (LAYOUT_MODES.includes(message.layout)) {
        if (message.layout !== layoutMode) {
          layoutMode = message.layout;
          normaliseForLayout();
          relayoutAll();
        }
      }
      break;

    case 'modifiers':
      /* Only sent while the bar is on 'auto'. */
      if (logoHeld !== !!message.logo) {
        logoHeld = !!message.logo;
        relayoutAll();
      }
      break;

    case 'output.layout':
      syncOutputs(message.outputs);
      send({ type: 'view.query' });
      break;

    case 'view.added':
      addView(message);
      break;

    /* A client outside the shell — an external bar, over `ext-workspace-v1` —
       asked for something to happen to a workspace. Nothing has happened in
       the compositor yet; see workspaceRequested(). */
    case 'workspace.request':
      workspaceRequested(message);
      break;

    case 'view.props': {
      const view = views.get(message.id);
      if (view) {
        view.title = message.title;
        view.app_id = message.app_id;
        renderBars();
      }
      break;
    }

    case 'view.removed':
      selectedIds.delete(message.id);
      removeView(message.id);
      break;

    case 'view.focused': {
      const nextId = message.id || null;
      if (nextId != null && !selectedIds.has(nextId)) {
        clearSelection();
      }
      focusedId = nextId;
      /* The whole state transition for the matrix layout: one splice, and the
         relayout at the bottom of this case reads the array as it now stands.
         Kept here rather than inside that layout's own plan because the
         history is what was focused *in what order*, which a plan built from
         the current focus alone cannot recover. */
      matrixFocused(focusedId);
      /* And the canvas pans to whatever was just focused, if it is not already
         on screen. Here rather than in its own plan for the same reason: the
         plan is a function of where the view is, and this is what moves it. */
      canvasFocused(focusedId);
      const found = focusedId != null ? findLeaf(focusedId) : null;
      if (found) {
        /* Focusing a window on a hidden workspace brings that workspace to a
         * monitor rather than leaving the user looking at nothing. */
        let host = hostOfWorkspace(found.workspace);
        if (host === null) {
          host = activeOutputName();
          const output = outputs.get(host);
          if (output) output.workspace = found.workspace;
        }
        setActiveOutput(host);
      }
      relayoutAll();
      break;
    }

    case 'status.update':
      /* Nothing in a status sample can change the workspace set, the window
         list or the focus, so the chrome is left exactly as it is. */
      lastStatus = message;
      renderBarsModules();
      break;

    case 'screencast.pick':
      /* Sent whole every time the highlight moves: the compositor owns the
         selection because it owns the keyboard. */
      showScreencastPicker(message);
      break;

    case 'screencast.pick.done':
      hideScreencastPicker(message.id);
      break;

    case 'notification.add':
      showNotification(message);
      break;
    case 'notification.close':
      /* The application withdrew it, so nothing is sent back. */
      dropNotification(message.id, false);
      break;

    case 'session.restore':
      restoreSession(message.state);
      break;

    case 'shell.command':
      handleShellCommand(message.command, message.args ?? []);
      /* Most commands rearrange something; saving is debounced, so asking on
         every one of them costs a timer reset. */
      saveSession();
      break;

    case 'error':
      console.error(`viewport: ${message.context}: ${message.message}`);
      break;
  }
});

window.addEventListener('resize', relayoutAll);

send({ type: 'output.query' });
/* Before view.query: the layout has to be in place as slots before the windows
   that fill them are replayed, or every one of them lands in a default
   position first and the restore has nothing left to do. */
send({ type: 'session.query' });
send({ type: 'view.query' });
