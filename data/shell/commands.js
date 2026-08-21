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
    case 'clipboard':
      toggleClipboard();
      break;
    /* The launcher. Bound to Mod4+d by default — the shell verb rather than
       an exec, for the reason the clipboard picker is one: the compositor
       keeps the list and the page draws it. */
    case 'launcher':
      toggleLauncher();
      break;
    /* The notification centre — what was notified while nobody was looking.
       A shell verb rather than a compositor action for the same reason the
       clipboard picker is one: the compositor keeps the list, the page draws
       it. */
    case 'notifications':
      toggleNotificationCentre();
      break;
    /* The two radios. Bound to Mod4+Shift+n and Mod4+Shift+t, and reachable
       by clicking the bar's network module — the same two ways in the
       clipboard and the power picker have between them. */
    case 'network':
      toggleNetworkPicker();
      break;
    case 'bluetooth':
      toggleBluetoothPicker();
      break;
    /* The on-screen keyboard. Bound to Mod4+Shift+k for a desk that has a
       real keyboard and wants to raise it anyway; a touch-only desk never
       needs the chord, because `osk.wanted` already brings it up. See
       osk.js. */
    case 'osk':
      toggleOsk();
      break;

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
      resizeByDelta(Number(args[0]), Number(args[1]), Number(args[2]), args[3]);
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
    /* The button that was driving any of the three above came up. Nothing
       moves; what ends is the gesture, and with it the coalescing and the
       suppressed animations. */
    case 'layout.drag.end':
      endGesture();
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

    /* Scrolling layout, and the canvas for `next`/`prev`. Bound only when the
       compositor is configured for one of them, but harmless to receive
       otherwise.

       Two layouts answer this because two layouts have windows the compositor
       cannot see: a column scrolled off the strip and a window panned off the
       plane are both reported as not on screen, and the compositor's own
       cycle walks what is on screen. Everywhere else Mod4+Tab stays where it
       has always been. */
    case 'layout.focus':
      clearSelection();
      if (layoutMode === 'canvas') {
        canvasFocusStep(arg !== 'prev');
      } else {
        scrollFocus(arg);
      }
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
    case 'canvas.fill':
      canvasFillFocused();
      break;
    case 'output.hdr':
      /* No state of its own: the compositor owns whether an output is in HDR,
         and toggling is asking it to flip whatever it currently has. */
      send({ type: 'output.hdr', name: activeOutputName() });
      break;
    case 'workspace.step':
      clearSelection();
      stepWorkspaceOnActive(Number(arg));
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
      /* 'auto', 'manual' or 'off' — see applyOskMode in osk.js for what
         changing it does to a keyboard already on screen. */
      applyOskMode(message.osk);
      /* Absent means on: a config file that says nothing should get the
         explanation, and only someone who has read it once turns it off. */
      document.documentElement.classList.toggle('no-logo',
        message.logo === false);
      document.documentElement.classList.toggle('no-tutorial',
        message.tutorial === false);
      /* The keymap as the compositor will really act on it, for the tutorial
         on an empty desktop. Absent from an older compositor, which leaves the
         two lines the markup ships with rather than emptying the box. */
      if (Array.isArray(message.binds)) {
        keybinds = message.binds.filter(
          (bind) => bind && typeof bind.chord === 'string'
            && typeof bind.action === 'string');
        renderKeybinds();
      }
      /* Something is being drawn behind the page — a terminal, as the
         wallpaper — so the gradient in `body` has to go, or it is painted over
         the thing it is meant to reveal. Absent means nothing is back there,
         which is every desktop that has not asked for one. */
      document.documentElement.classList.toggle('behind',
        message.background_terminal === true);
      /* The desktop background: a picture the compositor resolved to a URL, or
         a CSS value it passed through — `#1a1b26`, `rgb(...)`, a gradient.

         Which of the two decides which property it lands on, because a colour
         is not an image: a `background-image` of `#1a1b26` is nothing at all,
         and the desktop would come up black with the setting apparently
         ignored. Absent takes the class off and the shipped gradient comes
         back — that is what an empty `wallpaper` in the config file and
         `--path ''` over the socket both arrive as. */
      {
        const root = document.documentElement;
        /* A terminal behind the page wins over anything in it: the two cannot
           both be seen, since the page is what would cover the terminal, and
           the terminal is the one somebody asked for by running a program.
           Decided here rather than left to the order of two CSS rules, so that
           there is one place to read the answer off. */
        const value = typeof message.wallpaper === 'string'
          && message.wallpaper.trim() !== ''
          && message.background_terminal !== true
          ? message.wallpaper.trim() : null;
        /* An image is a `url()` or a gradient of any kind; a URL of its own is
           wrapped, quoted, because a resolved path is percent-encoded but may
           still hold a bracket or a comma that an unquoted url() would end at.
           Everything else is a colour. */
        let image = null;
        let colour = null;
        if (value !== null) {
          if (/^(url\(|[a-z-]*gradient\()/i.test(value)) {
            image = value;
          } else if (/^[a-z][a-z0-9+.-]*:\/\//i.test(value)) {
            image = `url("${value.replace(/"/g, '%22')}")`;
          } else {
            colour = value;
          }
        }
        root.classList.toggle('wallpaper', value !== null);
        if (image !== null) {
          root.style.setProperty('--wallpaper', image);
        } else {
          root.style.removeProperty('--wallpaper');
        }
        if (colour !== null) {
          root.style.setProperty('--wallpaper-color', colour);
        } else {
          root.style.removeProperty('--wallpaper-color');
        }
        /* One class per fitting rather than a data attribute, to match the
           rest of the shell's styling, and all four removed first so switching
           mode does not leave the last one on. `fill` is the default and has
           no class of its own; a colour has nothing to fit, so it gets none of
           them either. */
        for (const mode of WALLPAPER_MODES) {
          root.classList.toggle(`wallpaper-${mode}`,
            image !== null && message.wallpaper_mode === mode);
        }
      }
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
        }
      }
      if (LAYOUT_MODES.includes(message.layout)) {
        if (message.layout !== layoutMode) {
          layoutMode = message.layout;
          normaliseForLayout();
        }
      }
      /* And then lay the desktop out again, whatever changed.
       *
       * Everything above is either a style the browser reflows for or a value
       * the geometry pass reads — the gaps, the border's width, whether a lone
       * window is drawn square — and the compositor is told about all of it
       * through `view.layout`, which is only sent when a measurement differs
       * from the last one that was sent. Nothing measures on its own: a
       * desktop nobody is touching runs no pump, so a setting changed over the
       * control socket reached the page, changed the stylesheet, and then sat
       * there.
       *
       * That is what "the radius does nothing until I open another window"
       * was: opening one relayouts, the pass runs, and the values that had
       * been sitting in the page since the message arrived finally go out.
       * A config message is rare — a connect, a reload, a line typed at the
       * socket — so doing this unconditionally costs nothing anybody can
       * measure. */
      relayoutAll();
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

    /* Whose dialog this is, said after the fact. A dialog an application opens
       itself arrives with its parent on `view.added`; one that comes from the
       portal — a file chooser, in another process — is parented over
       xdg-foreign well after it has mapped, and until this message the shell
       has a window belonging to nothing. */
    case 'view.parent': {
      const view = views.get(message.id);
      if (view) {
        view.parent = Number.isFinite(message.parent) ? message.parent : null;
        /* The canvas placed it in the middle of the view for want of anything
           better; now there is something better. A no-op in every other
           layout, and for a window someone has since moved themselves. */
        if (canvasReparented(message.id)) relayoutAll();
      }
      break;
    }

    /* The client was configured at a different size from the one asked for,
       because it would not go as small as the layout wanted. The canvas holds
       a rectangle that *is* the client's size, so it has to take the correction
       — otherwise everything measured against that rectangle, a dialog centred
       on it most visibly, is out by the difference. */
    case 'view.configured':
      if (canvasConfigured(message.id, Number(message.width),
        Number(message.height))) {
        relayoutAll();
      }
      break;

    case 'view.removed':
      selectedIds.delete(message.id);
      removeView(message.id);
      break;

    case 'view.focused': {
      const nextId = message.id || null;
      if (nextId != null && !selectedIds.has(nextId)) {
        clearSelection();
      }
      /* A click on a window is given to the client, so the document listener
         at the bottom of this file never sees it. The focus moving is what
         that click looks like from here, and an open menu has to go with it. */
      closeTrayMenu();
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

    case 'shortcuts.pick':
      /* Sent once rather than per keypress: there is no highlight to move,
         because the answer is yes or no to the whole list. */
      showShortcutsPicker(message);
      break;

    case 'shortcuts.pick.done':
      hideShortcutsPicker(message.id);
      break;

    case 'clipboard.history':
      /* Sent on every copy and in answer to clipboard.query. Drawn only while
         the picker is open; see applyClipboard. */
      applyClipboard(message.entries ?? []);
      break;

    case 'launcher.list':
      /* Sent in answer to launcher.query — on open and on every keystroke of
         the filter. Drawn only while the picker is open; see applyLauncher. */
      applyLauncher(message.apps ?? []);
      break;

    case 'notification.history':
      /* Sent whenever the compositor's record changes and in answer to
         notification.list. Drawn only while the centre is open; see
         applyNotificationHistory. */
      applyNotificationHistory(message.entries ?? []);
      break;

    case 'mpris.update':
      /* Sent when it changes rather than on the status tick, so this redraws
         the widgets and nothing else: a track starting cannot move a window. */
      mprisPlayer = message.player ?? null;
      renderBarsWidgets();
      break;

    case 'power.update':
      applyPower(message);
      break;

    case 'osk.wanted':
      applyOskWanted(message.wanted === true);
      break;

    case 'network.update':
      /* Sent whenever NetworkManager changes its mind, which while a scan is
         running is several times a second. Drawn only while the picker is
         open; see applyNetwork. */
      applyNetwork(message);
      break;

    case 'bluetooth.update':
      applyBluetooth(message);
      break;

    case 'tray.menu':
      /* The compositor fetched the menu; the shell draws exactly what it was
         handed. Only items whose menu this compositor can read send one —
         the rest draw their own window and nothing arrives here. */
      showTrayMenu(message);
      break;

    case 'tray.update':
      /* The whole tray, every time any part of it changes; see the event's own
         note. Nothing else in a tray message can move a window, so this does
         not relayout. */
      applyTray(message.items ?? []);
      /* A tray that changed under an open menu is an application that
         restarted, exited or changed what it is offering, and the menu on
         screen is no longer the menu it published. */
      closeTrayMenu();
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

/* The desktop is not a web page, whatever it is made of.
 *
 * The shell is drawn by a browser engine, and a browser engine offers its own
 * menu on a right-click — back, reload, view source, save image. On a desktop
 * background that menu is nonsense: there is no page to go back to and nothing
 * to save, and it appears over the windows because it is the engine's own
 * surface rather than anything the compositor placed. Servo's is the one that
 * was noticed, but every backend has one.
 *
 * The right button is the compositor's here — Mod4 and it resize a window —
 * and where it is not, it belongs to whatever the pointer is over. Neither of
 * those is served by a menu about the document.
 *
 * On the document rather than on any element, so it holds for the whole of the
 * shell including anything drawn later, and in the capture phase so that a
 * handler of its own is not required to know about it. */
document.addEventListener('contextmenu', (event) => {
  event.preventDefault();
}, true);

/* A click anywhere else takes an open tray menu down, which is what a menu
 * does everywhere. Rows stop the event themselves, so this only ever sees the
 * clicks that missed.
 *
 * A click on a *window* never reaches here — the compositor gives it to the
 * client — so the menu is also closed when the focus moves, which is what a
 * click on a window causes. Between the two there is no way to leave one
 * stranded. */
document.addEventListener('click', () => {
  closeTrayMenu();
  /* And the clipboard picker, which is a menu in every way that matters: rows
     stop the event themselves, so this only sees the clicks that missed. */
  closeClipboard();
  /* And the launcher, on the same terms: the dialog stops the clicks that act
     — a row chosen, the field clicked for the caret — so what reaches here is
     a click on the desktop beyond it. Closing it also gives the keyboard back
     to the window that had it. */
  closeLauncher();
  /* And the notification centre, on the same terms. Its rows stop the clicks
     that act — forget, and an action button — so what reaches here is a click
     on the list's own background or beyond it. */
  closeNotificationCentre();
  /* And the two radio pickers, on the same terms. Closing them is not only
     tidiness: the network picker stops the scan and the Bluetooth one stops
     the discovery, so one left open by a click that missed is a radio left
     transmitting. The click that opens the network picker — on the bar's
     network module — stops itself for exactly this reason. */
  closeNetworkPicker();
  closeBluetoothPicker();
});

send({ type: 'output.query' });
/* Before view.query: the layout has to be in place as slots before the windows
   that fill them are replayed, or every one of them lands in a default
   position first and the restore has nothing left to do. */
send({ type: 'session.query' });
send({ type: 'view.query' });
