# Kiosk shell

One application, fullscreen, and nothing else. A departures board, a menu
screen, a point-of-sale terminal, an instrument panel.

It exists partly to be useful and partly to show how little of the protocol a
shell is obliged to implement. `data/shell/` is a desktop — workspaces, tiling,
a launcher, a bar, ten files. This is the same protocol answered in about two
hundred lines, and the interesting part is everything it declines to do.

```sh
viewport --config examples/kiosk/config.json
```

## What it does

The compositor launches one application from its `startup` command. The first
window that maps owns the screen and is drawn fullscreen with no frame; anything
that window opens afterwards — a print dialog, a file chooser, an authentication
prompt — is centred on top of it at the size it asked for. Dialogs are drawn
rather than refused, because a kiosk where printing silently does nothing is a
broken kiosk.

While no application window exists the screen says so, and says something
different before the application has ever appeared than after it has gone away.
Those are very different problems and they look identical otherwise.

Extra monitors say "this screen is not in use". One application window can only
be on one screen.

Every command the compositor forwards is ignored — there are no workspaces to
switch to, nothing to tile, no bar to toggle. The shell also never saves a
layout, because there is no layout.

## Locking it down

The shell is not what locks the machine down. It draws one window fullscreen;
it does not stop anything. The lockdown is in `config.json`, and it is worth
understanding which key does what.

**`"binds": {}` removes every keybinding.** Presence of a `binds` object
replaces the built-in keymap wholesale, so an empty one leaves no terminal, no
launcher, no exit and no workspace keys. This is the one place you want `binds`
rather than `binds_override` — override keeps the defaults it does not mention,
which is the opposite of the goal.

While you are still setting the machine up, keep one escape:

```json
"binds": { "Ctrl+Alt+Shift+x": "exit" }
```

**`"vt_switching": false` closes the console.** `Ctrl+Alt+F2` normally switches
virtual terminals, and it is checked before the config file, before the keymap
and before the shell — it is the one thing that still works when a shell never
paints or the compositor wedges. A public kiosk usually does want it gone: a
visitor reaching a login prompt is the threat. Read the next section before you
set it.

**`decorations: "server"`** stops clients drawing their own titlebars, so a
fullscreen application does not arrive wearing a close button.

**`idle.lock_after: 0`** disables the lock screen. A lock screen on an
unattended kiosk is a password prompt in a public place, which is worse than
whatever it was protecting.

## What this does not lock down

Read this part. A kiosk is a security posture, and the failure mode of getting
it wrong is a stranger with a shell prompt.

**Disabling VT switching does not remove the consoles.** It stops the compositor
handing over the keys. The getty units on the other VTs are still running and
still reachable by any other route. Disable them too.

**With VT switching off there is no way back from a wedged compositor.** Not the
keyboard, anyway. If the shell stops painting and the exit binding is gone, the
options are SSH and the power button. Get SSH working *before* you set this, not
after.

**The control socket is not authenticated beyond the user id.** Anything running
as the same user can drive the compositor over
`$XDG_RUNTIME_DIR/viewport-*.sock` — including telling it to quit. If the kiosk
application can be made to run an arbitrary command, the lockdown is over. Run
the kiosk as a dedicated unprivileged user with nothing else in its session.

**The application is its own attack surface, and usually the largest.** A
browser needs its own kiosk mode or it has a URL bar, a downloads panel and
`file:///`. Anything with a file chooser gives a visitor a filesystem browser.
Anything with a "help" menu may give them a browser. This is where most kiosk
escapes actually happen, and no compositor setting reaches it.

**Nothing here restarts the application.** `startup` runs once. Point it at a
systemd user unit with `Restart=always` — a restart loop needs backoff, logging,
and the judgement to give up, none of which belongs in a compositor. The shell
shows a waiting message while no window exists, so a supervised restart reads as
a brief message rather than a black screen.

**Physical access is still physical access.** USB, the power button, and the
boot order are all outside anything discussed here.

## Adapting it

`kiosk.js` starts with a small `KIOSK` object: which `app_id` owns the screen,
and the three strings shown when nothing is running.

Setting `app` is worth doing if you know the id. With the default of `null` the
first window to map wins, and an application that shows a splash screen before
its real window can leave the splash owning the screen for the whole session.
Run once with `--debug` and read the `app_id` out of the log.

To show something on the other monitors — a second view, a clock, a logo —
`render()` is the one place that decides what a non-primary screen contains.
