/* A notification extension: it decorates the popup the shell built with a small
 * urgency badge in the head, and (optionally) keeps low-urgency popups off
 * screen. It does not replace the notification machinery — the shell still
 * draws the popup, routes its actions and keeps the history; this only adds to
 * the element it is handed.
 *
 * Enable it in the config file:
 *
 *   {
 *     "notification_extensions": { "badges": "notifications/badges.js" },
 *     "notification_extension": "badges"
 *   }
 *
 * The path resolves beside the config file. A notification extension may define
 * `filter(notification)` (return false to keep its popup off screen) and/or
 * `render({ notification, el })` (decorate the popup element). Both are
 * optional; a descriptor with neither is refused.
 */
registerNotification('badges', {
  /* Low-urgency popups (urgency 0) are the ones applications mark "don't
   * interrupt" — keep them in the centre but off the screen. Comment this out
   * to show every popup. */
  filter(notification) {
    return (notification.urgency ?? 1) > 0;
  },

  render({ notification, el }) {
    const urgency = notification.urgency ?? 1;
    const label = urgency >= 2 ? 'critical' : urgency === 0 ? 'low' : 'normal';
    const head = el.querySelector('.notification-head');
    if (!head) return;
    const badge = document.createElement('span');
    badge.className = `notification-badge urgency-${urgency}`;
    badge.textContent = label;
    /* Before the close button, after the app name. */
    const close = head.querySelector('.notification-close');
    if (close) head.insertBefore(badge, close);
    else head.append(badge);
  },
});
