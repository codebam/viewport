/* Example user widget: the time in an IANA timezone. Loaded only when a
   bar_widgets or bar_items entry of type "custom" names 'world-clock' and
   widget_extensions points that name at this file. */
registerWidget('world-clock', {
  /* Called once, when the bar first places the widget. The shell has already
     made the outer <span class="module widget">; build the widget's own
     markup inside it here. */
  mount(el) {
    const time = document.createElement('span');
    time.className = 'world-clock-time';
    el.appendChild(time);
  },

  /* Called right after mount, then on every status tick — about every two
     seconds, which is often enough for a clock. Re-reading ctx.options each
     tick means a reload picks up a changed zone with no state to reset. */
  update(el, ctx) {
    const time = el.querySelector('.world-clock-time');
    if (!time) return;
    const zone = ctx.options.zone
      || Intl.DateTimeFormat().resolvedOptions().timeZone;
    let text;
    try {
      text = new Intl.DateTimeFormat(undefined, {
        timeZone: zone,
        hour: '2-digit',
        minute: '2-digit',
        hourCycle: 'h23',
      }).format(new Date());
    } catch {
      /* Intl throws a RangeError for a zone it does not know. Show the bad
         name rather than a blank widget, and keep the error out of the bar. */
      text = `${zone} ?`;
    }
    if (time.textContent !== text) time.textContent = text;
  },
});
