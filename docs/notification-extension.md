# Notification extensions

A notification extension is a user script that draws or filters notification
popups. It is the fourth member of the extension family — layout, widget,
wallpaper, notification — and mirrors their shape: a manifest in the config
file, a `registerNotification` call in the script, and a lifecycle the shell
drives. The shell still owns the notification machinery (the popup, its actions,
the centre, the history); the extension decorates or suppresses the popup, it
does not replace any of that.

Like the other extensions, the script runs in the shell page, so it works under
every compositor renderer (Vulkan included) with no GPU shader path.

## The manifest and selecting one

Declare each script under the name it registers, then select that name with
`notification_extension`:

```json
{
  "notification_extensions": {
    "badges": "notifications/badges.js"
  },
  "notification_extension": "badges"
}
```

Paths are local files. Relative paths resolve beside the config file; `~/` and
absolute paths also work. Remote URLs are rejected: this is code the shell runs
inside the desktop. The compositor validates the manifest, resolves each path to
a `file://` URL, and sends the resolved list to the shell in the `config` event
as `notification_extensions: [{name, url}]`. A path that does not exist is still
sent through, so the shell ends up with a name nothing registered: the popups
fall back to the shell's own rather than aborting, the same bargain a missing
layout script strikes.

Names may contain ASCII letters, digits, `_` and `-`. Setting
`notification_extension` to `""` (or removing it) returns to the shell's own
popups.

## Writing one

A script registers exactly one notification extension with
`registerNotification`:

```js
registerNotification('badges', {
  // Optional. Return false to keep this notification's popup off screen. The
  // notification still reaches the centre and the history — only the popup is
  // filtered. Absent shows every popup.
  filter(notification) {
    return (notification.urgency ?? 1) > 0;
  },

  // Optional. Decorate the popup element the shell built. `notification` is the
  // message (app_name, summary, body, urgency, actions, ...); `el` is the popup
  // div, already populated with the head, summary, body and actions.
  render({ notification, el }) {
    const badge = document.createElement('span');
    badge.className = 'notification-badge';
    badge.textContent = notification.urgency >= 2 ? 'critical' : 'normal';
    el.querySelector('.notification-head')?.append(badge);
  },
});
```

At least one of `filter` or `render` is required; a descriptor with neither does
nothing and is refused at registration. Both are contained: a throw is logged
and the popup shows as the shell built it, so a bad extension cannot silence the
desktop or take a popup down.

`render` runs after the shell has built the popup and before it is shown, so it
can add to or restyle the element. It must not replace the shell's action
handlers — the close button, the action buttons and the body click already send
the right `notification.*` messages. Treat the notification body as text: the
shell sets it with `textContent`, and rendering it as markup would let any
application that can send a notification run script in the desktop.

A working demo lives at
[`examples/notifications/badges.js`](../examples/notifications/badges.js): an
urgency badge in the popup head, with low-urgency popups filtered off screen.

## Lifecycle

Extension scripts load in name order when the `config` event arrives, before the
selected one is named. Changing the manifest or a script takes effect on the
next config reload. The selected extension's `filter` and `render` are called
per notification as it arrives; there is nothing to apply at selection time, so
switching the selection changes only the popups that arrive afterwards — ones
already on screen keep the decoration they were drawn with.