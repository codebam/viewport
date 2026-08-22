/* SPDX-License-Identifier: MIT
 *
 * The clock's format, and the calendar under it.
 *
 * One file for the two because they are one thing. The clock used to pass the
 * literal `'en-US'` to `toLocaleDateString` and assemble the time out of
 * `getHours()`, so every desk in the world read an American date and a
 * twenty-four-hour clock whether or not that is how it writes one — and a
 * calendar hung under a clock like that would be a bigger wrong thing than the
 * clock was on its own, with a German month under an American date and a week
 * starting on the wrong day. So the locale is decided once, here, and the
 * module, the month name, the weekday headings and the first column of the
 * grid all come out of it.
 *
 * Nothing in this file asks the compositor anything, and nothing needed adding
 * on that side to draw it: a month is arithmetic a page can do, and the names
 * are the engine's own `Intl` data. That data is the one thing here that can
 * be missing — the shell is drawn by whichever engine the backend names, and
 * a build with a trimmed ICU has weekday names for English and nothing else —
 * so every call into it goes through clockFormat()/clockPart(), which answer
 * null rather than throwing, and every caller has something readable to fall
 * back to. A clock is the one module on the bar that must never be blank.
 *
 * The dropdown itself is the tray menu's shape rather than the pickers': it is
 * anchored under the thing that was clicked, positioned in layout coordinates,
 * clamped to the monitor it opened on, and taken down by the document's own
 * click listener in commands.js. The pickers are centred dialogs because what
 * they ask about belongs to the machine; a calendar belongs to the clock it
 * hangs off, and on a two-monitor desk it belongs to the clock on *that*
 * monitor.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* ------------------------------------------------------------------------
 * The clock's format
 * --------------------------------------------------------------------- */

/* Weekday and month names for an engine whose `Intl` cannot supply them.
 *
 * English, and deliberately not apologised for: this is the last resort, it is
 * what the shell printed before any of this existed, and a bar reading
 * "Fri 22 Aug" is a bar somebody can still use. What it is not is the default
 * — see clockText(), which reaches the engine first every time. */
const CLOCK_DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
const CLOCK_DAYS_LONG = ['Sunday', 'Monday', 'Tuesday', 'Wednesday',
  'Thursday', 'Friday', 'Saturday'];
const CLOCK_MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
  'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const CLOCK_MONTHS_LONG = ['January', 'February', 'March', 'April', 'May',
  'June', 'July', 'August', 'September', 'October', 'November', 'December'];

/* Built formatters, keyed by locale and options.
 *
 * `Intl.DateTimeFormat` is not cheap to construct — it resolves a locale and
 * loads its data — and clockText() runs once a second for as long as the
 * session is up, on a desk that is otherwise asleep. A format string can ask
 * for half a dozen of them in one pass. Building each one once and keeping it
 * is the difference between a clock and a clock that costs measurable CPU to
 * stand still.
 *
 * Bounded by the number of distinct option shapes this file asks for, which is
 * a handful; cleared when the config changes so a locale nobody is using any
 * more does not sit here for the rest of the session. */
const clockFormatters = new Map();

/* The config file's `clock` block, as the compositor sent it.
 *
 * Absent — or a build too old to send one — is every field null, which is the
 * shipped behaviour: the engine's own locale, the hour that locale writes, and
 * the shape below. That is why an absent block cannot be forwarded as three
 * nulls from the compositor either; there is no constant on that side to send.
 *
 * A locale is checked here rather than trusted, because a malformed language
 * tag makes `Intl.DateTimeFormat` throw a RangeError — once a second, from a
 * timer, for the rest of the session, taking the clock with it. */
function applyClock(clock) {
  const next = { locale: null, hour12: null, format: null };
  if (clock && typeof clock === 'object') {
    const locale = typeof clock.locale === 'string' ? clock.locale.trim() : '';
    if (locale !== '') next.locale = supportedClockLocale(locale);
    if (typeof clock.hour12 === 'boolean') next.hour12 = clock.hour12;
    if (typeof clock.format === 'string' && clock.format !== '') {
      next.format = clock.format;
    }
  }
  clockConfig = next;
  clockFormatters.clear();
  /* The bar first, because a config arriving mid-session is a reload and the
     clock on screen is the old one until something redraws it. The calendar
     only if it is up — its month names came from the locale that just
     changed. */
  renderClocks();
  if (calendarOpen) renderCalendar();
}

/* The tag if the engine can make a formatter with it, null if it cannot.
 *
 * Constructing one is the only honest test. `supportedLocalesOf` answers a
 * different question — whether there is *data* for the tag — and its empty
 * answer is fine here: an engine with no Swedish falls back to its own locale
 * and draws something, which is better than refusing a tag that is perfectly
 * well formed. What must not get through is the tag that throws. */
function supportedClockLocale(tag) {
  try {
    new Intl.DateTimeFormat(tag);
    return tag;
  } catch (e) {
    console.warn(`clock.locale ${JSON.stringify(tag)} is not a language tag`
      + ' this engine can parse; using the session locale');
    return null;
  }
}

/* A formatter for these options, or null if the engine cannot make one.
 *
 * Null rather than a throw because every caller here has a fallback and none
 * of them has anywhere to report a failure to: this is a clock on a bar. */
function clockFormatter(options) {
  const key = `${clockConfig.locale ?? ''} ${JSON.stringify(options)}`;
  if (clockFormatters.has(key)) return clockFormatters.get(key);
  let formatter = null;
  try {
    formatter = new Intl.DateTimeFormat(clockConfig.locale ?? undefined, options);
  } catch (e) {
    formatter = null;
  }
  clockFormatters.set(key, formatter);
  return formatter;
}

/* What the locale writes for this date under these options, or null. */
function clockFormat(now, options) {
  const formatter = clockFormatter(options);
  if (!formatter) return null;
  try {
    const text = formatter.format(now);
    return typeof text === 'string' && text !== '' ? text : null;
  } catch (e) {
    return null;
  }
}

/* One named piece of a formatted date — the month name on its own, the AM/PM
 * on its own — or null.
 *
 * `formatToParts` is what makes a strftime possible against `Intl` at all:
 * `%B` is the month name and nothing else, and pulling it out of a formatted
 * string would mean guessing where the locale put it. Older engines have
 * `format` and not this, hence the guard and hence every caller's fallback. */
function clockPart(now, options, type) {
  const formatter = clockFormatter(options);
  if (!formatter || typeof formatter.formatToParts !== 'function') return null;
  try {
    const part = formatter.formatToParts(now).find((p) => p.type === type);
    return part && part.value !== '' ? part.value : null;
  } catch (e) {
    return null;
  }
}

/* The twelve-or-twenty-four-hour choice, as options to hand `Intl`.
 *
 * `hourCycle: 'h23'` rather than `hour12: false` for the twenty-four-hour
 * case, because the two are not the same thing: `hour12: false` is specified
 * to fall back to `h24` for a locale whose default cycle is `h12`, and `h24`
 * writes midnight as 24:07. Engines have been fixed and unfixed on this for
 * years; naming the cycle is the version that cannot say 24:07.
 *
 * Absent is neither, which leaves the locale to say — a desk that sets nothing
 * but `locale: "en-GB"` gets a 24-hour clock because that is what en-GB
 * writes, and that is the whole point of the key. */
function clockHourOptions() {
  if (clockConfig.hour12 === true) return { hour12: true };
  if (clockConfig.hour12 === false) return { hourCycle: 'h23' };
  return {};
}

/* What the clock module says.
 *
 * With no `format` in the config: the clock glyph, then the date and the time
 * as one `Intl` call rather than two joined with a comma. One call is what
 * gets the *order* right — "Fri, Aug 22, 2:15 PM", "Fr., 22. Aug., 14:15",
 * "8月22日(金) 14:15" — and joining two formatted halves by hand would be the
 * same mistake as hardcoding en-US, one level down.
 *
 * With a `format`: the template, expanded by clockStrftime(). The glyph is not
 * added — a template is the whole module, which is how somebody drops the
 * glyph, and adding one they did not ask for would leave no way to. */
function clockText(now = new Date()) {
  if (clockConfig.format !== null) return clockStrftime(clockConfig.format, now);
  const text = clockFormat(now, {
    weekday: 'short',
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    ...clockHourOptions(),
  });
  return `󰥔 ${text ?? clockFallbackText(now)}`;
}

/* The clock for an engine that could not format one: the exact string this
 * shell printed before any of this existed, in English, with the hour drawn
 * the way the config asked for it. Nobody should see this. */
function clockFallbackText(now) {
  const date = `${CLOCK_DAYS[now.getDay()]}, ${CLOCK_MONTHS[now.getMonth()]}`
    + ` ${clockPad(now.getDate())}`;
  if (clockConfig.hour12 === true) {
    return `${date}, ${clockHour12(now)}:${clockPad(now.getMinutes())}`
      + ` ${now.getHours() < 12 ? 'AM' : 'PM'}`;
  }
  return `${date}, ${clockPad(now.getHours())}:${clockPad(now.getMinutes())}`;
}

function clockPad(n, width = 2, fill = '0') {
  return String(n).padStart(width, fill);
}

/* Midnight is 12, not 0, and noon is 12 as well. */
function clockHour12(now) {
  return now.getHours() % 12 === 0 ? 12 : now.getHours() % 12;
}

/* A strftime template, expanded.
 *
 * strftime rather than a set of booleans, and rather than a scheme of this
 * shell's own, for two reasons. The thing people actually want to change is
 * the *arrangement* — seconds on or off, the date after the time, no glyph,
 * two spaces between the halves — and no list of flags ever finishes covering
 * that. And every status bar a person arrives here from spells it this way,
 * so the string in their waybar config is the string that works.
 *
 * The conversions that name something — `%A`, `%B`, `%p`, `%x`, `%Z` — go
 * through the locale, so a template is a layout and not a second place to
 * write English into the shell.
 *
 * An unknown conversion is copied through with its percent sign rather than
 * dropped: a typo that shows as `%Q` is one somebody can find, and one that
 * silently ate the next character is not. A trailing bare `%` is likewise
 * itself. */
function clockStrftime(format, now) {
  let out = '';
  for (let i = 0; i < format.length; i++) {
    if (format[i] !== '%' || i === format.length - 1) {
      out += format[i];
      continue;
    }
    i++;
    out += clockConversion(format[i], now);
  }
  return out;
}

function clockConversion(spec, now) {
  const hour = clockHourOptions();
  switch (spec) {
    case '%': return '%';
    /* Literal escapes, because a config file is JSON and a newline in one is
       already an escape — but a tab in one is worth a name. */
    case 'n': return '\n';
    case 't': return '\t';

    /* The named things, each with the English table behind it. */
    case 'a':
      return clockPart(now, { weekday: 'short' }, 'weekday')
        ?? CLOCK_DAYS[now.getDay()];
    case 'A':
      return clockPart(now, { weekday: 'long' }, 'weekday')
        ?? CLOCK_DAYS_LONG[now.getDay()];
    case 'b': case 'h':
      return clockPart(now, { month: 'short' }, 'month')
        ?? CLOCK_MONTHS[now.getMonth()];
    case 'B':
      return clockPart(now, { month: 'long' }, 'month')
        ?? CLOCK_MONTHS_LONG[now.getMonth()];
    /* AM/PM as the locale writes it — which for a locale that does not write
       one at all is the empty string, and that is the right answer rather than
       a missing one: `%p` in a Japanese template should not print "PM". The
       fallback is only for an engine with no `formatToParts`. */
    case 'p':
      return (clockPart(now, { hour: 'numeric', hour12: true }, 'dayPeriod')
        ?? (now.getHours() < 12 ? 'AM' : 'PM')).toUpperCase();
    case 'P':
      return (clockPart(now, { hour: 'numeric', hour12: true }, 'dayPeriod')
        ?? (now.getHours() < 12 ? 'AM' : 'PM')).toLowerCase();
    case 'Z':
      return clockPart(now, { timeZoneName: 'short' }, 'timeZoneName') ?? '';

    /* The locale's own three shapes, for a template that wants the date
       written however this desk writes dates and nothing more said about
       it. */
    case 'c':
      return clockFormat(now, { dateStyle: 'medium', timeStyle: 'medium', ...hour })
        ?? clockFallbackText(now);
    case 'x':
      return clockFormat(now, { dateStyle: 'short' })
        ?? `${clockPad(now.getFullYear() % 100)}-${clockPad(now.getMonth() + 1)}`
          + `-${clockPad(now.getDate())}`;
    case 'X':
      return clockFormat(now, { timeStyle: 'medium', ...hour })
        ?? `${clockPad(now.getHours())}:${clockPad(now.getMinutes())}`
          + `:${clockPad(now.getSeconds())}`;

    /* Numbers, which are the same in every locale this shell draws — the
       digits themselves are not, in an Arabic or Devanagari locale, but a
       template asking for `%Y-%m-%d` is asking for a machine-readable date
       and getting one. Anybody wanting the locale's own digits has `%x`. */
    case 'd': return clockPad(now.getDate());
    case 'e': return clockPad(now.getDate(), 2, ' ');
    case 'j': return clockPad(clockDayOfYear(now), 3);
    case 'm': return clockPad(now.getMonth() + 1);
    case 'y': return clockPad(now.getFullYear() % 100);
    case 'Y': return String(now.getFullYear());
    case 'H': return clockPad(now.getHours());
    case 'k': return clockPad(now.getHours(), 2, ' ');
    case 'I': return clockPad(clockHour12(now));
    case 'l': return clockPad(clockHour12(now), 2, ' ');
    case 'M': return clockPad(now.getMinutes());
    case 'S': return clockPad(now.getSeconds());
    case 's': return String(Math.floor(now.getTime() / 1000));
    /* The offset as `+0100`, from the minutes-behind-UTC the engine reports —
       which is signed the other way round, so Berlin in summer is -120. */
    case 'z': {
      const offset = -now.getTimezoneOffset();
      const sign = offset < 0 ? '-' : '+';
      const abs = Math.abs(offset);
      return `${sign}${clockPad(Math.floor(abs / 60))}${clockPad(abs % 60)}`;
    }

    /* The compounds, spelled out rather than recursed into, so the table above
       stays the one place a conversion is defined. */
    case 'F':
      return `${now.getFullYear()}-${clockPad(now.getMonth() + 1)}`
        + `-${clockPad(now.getDate())}`;
    case 'D':
      return `${clockPad(now.getMonth() + 1)}/${clockPad(now.getDate())}`
        + `/${clockPad(now.getFullYear() % 100)}`;
    case 'R': return `${clockPad(now.getHours())}:${clockPad(now.getMinutes())}`;
    case 'T':
      return `${clockPad(now.getHours())}:${clockPad(now.getMinutes())}`
        + `:${clockPad(now.getSeconds())}`;

    default: return `%${spec}`;
  }
}

function clockDayOfYear(now) {
  const start = new Date(now.getFullYear(), 0, 1);
  /* Whole days between two local midnights, so a date in a month where the
     clocks changed is not 0.958 of a day out. */
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  return Math.round((today - start) / 86400000) + 1;
}

/* ------------------------------------------------------------------------
 * The calendar
 * --------------------------------------------------------------------- */

/* How wide the dropdown is drawn, in layout pixels. Fixed rather than measured
 * because the compositor is told a rectangle and that rectangle has to be
 * decided before the grid is in the document — the same bargain the tray menu
 * makes. Seven columns and a little padding. */
const CALENDAR_WIDTH = 238;

/* Which regions start their week on a day other than Monday, for an engine
 * with no `weekInfo`.
 *
 * ISO 8601 says Monday and most of CLDR's world agrees, so these are the
 * exceptions — a subset of CLDR's own two lists, kept to the regions with
 * enough desks behind them to be worth a line. Everything absent is Monday,
 * which is the safe way for this to be wrong: it is what the standard says.
 *
 * Two lists rather than one flag, because there are three answers and not two.
 * A binary "Sunday or Monday" would put the Saturday-first countries on the
 * wrong day of the week every week of the year, which is precisely the failure
 * the hardcoded 'en-US' was.
 *
 * Engines that answer `getWeekInfo()` never reach this; see
 * calendarFirstDay(). */
const CALENDAR_SUNDAY_FIRST = new Set([
  'US', 'CA', 'MX', 'BR', 'CO', 'PE', 'VE', 'DO', 'GT', 'PA', 'JM', 'PR',
  'JP', 'KR', 'TW', 'HK', 'MO', 'CN', 'SG', 'TH', 'ID', 'IN', 'PK', 'BD',
  'PH', 'IL', 'SA', 'YE', 'ZA', 'KE', 'ZW', 'MZ', 'PT', 'MT',
]);
const CALENDAR_SATURDAY_FIRST = new Set([
  'AE', 'AF', 'BH', 'DZ', 'EG', 'IQ', 'IR', 'JO', 'KW', 'LY', 'OM', 'QA',
  'SD', 'SY',
]);

/* Open the calendar under `anchor`, or take it down if it is already up.
 *
 * The anchor is the clock module that was clicked, so on a two-monitor desk
 * the calendar appears under the clock that was pointed at rather than under
 * some notional first one. A `shell calendar` binding has clicked nothing, and
 * falls back to the clock on the output being looked at. */
function toggleCalendar(anchor = null) {
  if (calendarOpen) {
    closeCalendar();
    return;
  }
  calendarOpen = true;
  calendarAnchor = anchor ?? activeClockElement();
  /* Always today's month on open. A calendar reopened three months forward
     because that is where it was left is one that has to be read before it can
     be trusted, which is the opposite of what a glance at a clock is for. */
  calendarMonth = null;
  renderCalendar();
}

function closeCalendar() {
  if (!calendarOpen) return;
  calendarOpen = false;
  calendarAnchor = null;
  calendarMonth = null;
  calendarDrawnDay = '';
  calendarEl.replaceChildren();
  calendarEl.hidden = true;
  setOverlay('calendar', null);
}

/* The clock module on the output being looked at, for an opening that came
 * from a key rather than from a pointer. Null when the bar has been overridden
 * with a `bar_items` list that has no clock in it, which renderCalendar()
 * handles by hanging the grid off the top corner of the output instead. */
function activeClockElement() {
  return outputs.get(activeOutputName())?.modules?.clock ?? null;
}

/* Step the month on view. Today's month is where a null starts from, so the
 * arrows work on a calendar that has never been paged. */
function stepCalendar(delta) {
  const view = calendarMonth ?? calendarMonthOf(new Date());
  /* `new Date(y, 12, 1)` is January of the next year and `new Date(y, -1, 1)`
     is December of the previous one, which is the whole of the year rollover:
     doing this arithmetic by hand is how a calendar ends up with a month 13. */
  const stepped = new Date(view.year, view.month + delta, 1);
  calendarMonth = calendarMonthOf(stepped);
  renderCalendar();
}

function calendarMonthOf(date) {
  return { year: date.getFullYear(), month: date.getMonth() };
}

/* The first day of the week as the locale writes it: 0 for Sunday through 6
 * for Saturday, which is what `Date.getDay()` speaks.
 *
 * `getWeekInfo()` is the answer where there is one — it is CLDR's own, per
 * region, and it knows that en-GB starts on Monday while en-US starts on
 * Sunday, which no amount of parsing the language half of a tag would tell
 * anybody. Older engines have it as a `weekInfo` property instead, and the
 * oldest have neither, which is what the table above is for. */
function calendarFirstDay() {
  const tag = clockConfig.locale ?? resolvedClockLocale();
  try {
    const locale = new Intl.Locale(tag);
    const info = typeof locale.getWeekInfo === 'function'
      ? locale.getWeekInfo() : locale.weekInfo;
    /* CLDR counts Monday as 1 and Sunday as 7; getDay() counts Sunday as 0.
       A modulo is the whole conversion. */
    if (info && Number.isFinite(info.firstDay)) return info.firstDay % 7;
    /* `maximize()` is what turns "de" into "de-Latn-DE": a tag with no region
       in it still implies one, and the tables below are regional. */
    const region = locale.maximize?.().region ?? locale.region;
    if (region) return calendarFirstDayOfRegion(region);
  } catch (e) {
    /* No `Intl.Locale` at all, or a tag it will not take. Fall through to the
       region guess below, which works off the string. */
  }
  /* The subtags after the language, which is where a region can be — and only
     after it, because "en" uppercased is a two-letter string that is not a
     country. Without that slice, en-US read as the region EN and got Monday. */
  const region = String(tag).toUpperCase().split(/[-_]/).slice(1).find(
    (part) => part.length === 2 && /^[A-Z]+$/.test(part));
  return region ? calendarFirstDayOfRegion(region) : 1;
}

function calendarFirstDayOfRegion(region) {
  if (CALENDAR_SUNDAY_FIRST.has(region)) return 0;
  if (CALENDAR_SATURDAY_FIRST.has(region)) return 6;
  return 1;
}

/* The locale the engine is actually running under, for the case where the
 * config named none. Falls back to a tag rather than to undefined because the
 * callers above want something to read a region out of. */
function resolvedClockLocale() {
  try {
    return new Intl.DateTimeFormat().resolvedOptions().locale || 'en-US';
  } catch (e) {
    return 'en-US';
  }
}

/* Draw it. Rebuilt from nothing each time, rather than kept and updated the
 * way the bar's own modules are: this is drawn when it opens, when a month is
 * stepped and when midnight passes under it — never on a timer — and
 * forty-two cells is nothing to build. */
function renderCalendar() {
  if (!calendarOpen) return;
  const now = new Date();
  const view = calendarMonth ?? calendarMonthOf(now);
  calendarDrawnDay = calendarDayKey(now);

  calendarEl.replaceChildren();
  calendarEl.hidden = false;

  const panel = document.createElement('div');
  panel.className = 'calendar-panel';
  /* A click inside stays inside, on the notification centre's terms rather
     than the clipboard picker's: a calendar is mostly text, so leaving this to
     the rows would make reading a date a way to dismiss the thing it is
     written on. The arrows stop their own clicks as well, because they are
     inside this and a stopped event has to be stopped where it starts to keep
     the month from being stepped and the panel closed by one press. */
  panel.addEventListener('click', (e) => e.stopPropagation?.());

  panel.append(calendarHeader(view), calendarGrid(view, now),
    calendarFooter(now));
  calendarEl.append(panel);

  positionCalendar();

  /* The shell is one buffer under every window, so the calendar is behind them
     until the compositor is told where it is — see setOverlay in state.js. The
     docking element is the panel here: unlike the pickers there is no box
     spanning the output, because this hangs off a point rather than being
     centred on a screen. */
  setOverlay('calendar', calendarEl);
}

/* Under the clock that was clicked, and on the monitor that clock is on.
 *
 * Layout coordinates throughout, because that is what the compositor is told
 * when the rectangle is named: the page spans every output, so a position on
 * it is a position on the desk. Clamped horizontally for the reason the tray
 * menu is — a clock is the rightmost thing on the bar, so a panel starting at
 * its left edge runs off the screen — and given a maximum height rather than
 * being flipped above the bar, because the height is not known until the grid
 * has been laid out and a rectangle has to be reported now. */
function positionCalendar() {
  const rect = calendarAnchor?.getBoundingClientRect?.();
  const output = outputs.get(activeOutputName());
  const fallback = output?.rect
    ? { left: output.rect.x, top: output.rect.y, width: 0, height: 28 }
    : { left: 0, top: 0, width: 0, height: 28 };
  const at = rect ?? fallback;

  const bounds = outputBoundsAt(at.left, at.top);
  /* Centred under the module rather than left-aligned with it: the clock is a
     narrow thing at the end of the bar, and a panel four times its width
     hanging off one of its corners looks like it belongs to something else. */
  const wanted = at.left + at.width / 2 - CALENDAR_WIDTH / 2;
  const left = Math.max(bounds.left + 4,
    Math.min(wanted, bounds.right - CALENDAR_WIDTH - 4));
  const top = at.top + at.height + 4;

  calendarEl.style.left = `${Math.round(left)}px`;
  calendarEl.style.top = `${Math.round(top)}px`;
  calendarEl.style.width = `${CALENDAR_WIDTH}px`;
  calendarEl.style.maxHeight = `${Math.max(160, bounds.bottom - top - 8)}px`;
}

/* The month being looked at, with an arrow either side of it. */
function calendarHeader(view) {
  const head = document.createElement('div');
  head.className = 'calendar-head';

  head.append(calendarArrow('‹', 'previous month', -1));

  const title = document.createElement('span');
  title.className = 'calendar-title';
  const when = new Date(view.year, view.month, 1);
  title.textContent = clockFormat(when, { month: 'long', year: 'numeric' })
    ?? `${CLOCK_MONTHS_LONG[view.month]} ${view.year}`;
  head.append(title);

  head.append(calendarArrow('›', 'next month', 1));
  return head;
}

function calendarArrow(glyph, label, delta) {
  const button = document.createElement('button');
  button.className = 'calendar-step';
  button.textContent = glyph;
  button.title = label;
  button.addEventListener('click', (e) => {
    /* Stopped here as well as on the panel: the document's listener closes
       every dropdown on a click that is not one of theirs, and a panel that
       took itself down on the first press of an arrow would make paging
       impossible. */
    e.stopPropagation?.();
    stepCalendar(delta);
  });
  return button;
}

/* The weekday headings and the days, as one grid.
 *
 * Always six rows of seven. A month needs five rows or six depending on which
 * day it starts on, and letting the box change height would move the whole
 * panel — and the rectangle the compositor draws it in — as somebody pages
 * through the year. The days either side are drawn rather than left blank, so
 * the row that straddles two months reads as a week. */
function calendarGrid(view, now) {
  const grid = document.createElement('div');
  grid.className = 'calendar-grid';

  const first = calendarFirstDay();
  for (let i = 0; i < 7; i++) {
    const cell = document.createElement('span');
    cell.className = 'calendar-weekday';
    cell.textContent = calendarWeekdayName((first + i) % 7);
    grid.append(cell);
  }

  /* Where the first of the month falls, expressed as how many days of the
     previous month come before it in that week. */
  const start = new Date(view.year, view.month, 1);
  const lead = (start.getDay() - first + 7) % 7;
  const today = calendarDayKey(now);

  for (let i = 0; i < 42; i++) {
    const date = new Date(view.year, view.month, 1 - lead + i);
    const cell = document.createElement('span');
    cell.className = 'calendar-day';
    if (date.getMonth() !== view.month) cell.classList.add('adjacent');
    /* Marked by a class *and* readable as a class-free difference: `.today`
       draws a filled pill, which is the only state in this grid and the one
       thing a glance is looking for. */
    if (calendarDayKey(date) === today) cell.classList.add('today');
    cell.textContent = String(date.getDate());
    grid.append(cell);
  }
  return grid;
}

/* A weekday's short name, from a week known to start on a Sunday — 7 January
 * 2024 was one, and any Sunday would do. The names come from the locale like
 * everything else here. */
function calendarWeekdayName(day) {
  const when = new Date(2024, 0, 7 + day);
  return clockPart(when, { weekday: 'short' }, 'weekday') ?? CLOCK_DAYS[day];
}

/* Today's date, written out, and the way back to it.
 *
 * Both jobs in one row on purpose. A calendar paged three months forward needs
 * a way home, and the thing somebody presses to get there is the thing that
 * says what today is — so the row is a statement when the calendar is on this
 * month and a button when it is not, and it never becomes a mystery arrow. */
function calendarFooter(now) {
  const footer = document.createElement('div');
  footer.className = 'calendar-foot';

  const button = document.createElement('button');
  button.className = 'calendar-today';
  button.textContent = clockFormat(now, {
    weekday: 'long', day: 'numeric', month: 'long', year: 'numeric',
  }) ?? `${CLOCK_DAYS_LONG[now.getDay()]}, ${now.getDate()} `
    + `${CLOCK_MONTHS_LONG[now.getMonth()]} ${now.getFullYear()}`;
  button.addEventListener('click', (e) => {
    e.stopPropagation?.();
    calendarMonth = null;
    renderCalendar();
  });
  footer.append(button);
  return footer;
}

/* A date as the day it is, with no time on it — what "is this cell today" and
 * "has midnight passed" both ask. Local, because a calendar is local: a UTC
 * key would turn over at the wrong moment everywhere but London in winter. */
function calendarDayKey(date) {
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

/* Called from the clock's own tick, once a second.
 *
 * Almost always nothing: it compares two short strings and returns. What it is
 * for is the calendar left open across midnight, which would otherwise go on
 * marking yesterday until somebody closed it — and the marked day is the one
 * thing on the grid anybody is reading. A month that has been paged away from
 * is redrawn too, since the pill may need to disappear from it. */
function refreshCalendarDay() {
  if (!calendarOpen) return;
  if (calendarDayKey(new Date()) === calendarDrawnDay) return;
  renderCalendar();
}

/* Taken down when the bar it hangs off leaves the screen.
 *
 * Under `bar: auto` the bar is only up while Mod4 is held, so the click that
 * opens the calendar is followed within a second by the key being released —
 * and a dropdown left pointing at a bar that is no longer drawn is a rectangle
 * the compositor keeps painting over the windows with nothing above it to
 * explain what it is. Called from the relayout that hides the bar; see
 * geometry.js. */
function closeCalendarOff(output) {
  if (!calendarOpen) return;
  if (calendarAnchor) {
    if (!isInsideOutput(calendarAnchor, output)) return;
  } else if (output.name !== activeOutputName()) {
    /* Opened from a key on a bar that has no clock element to hang off — see
       renderCalendar's fallback. It belongs to the output being looked at, so
       a different monitor's bar going away is not about it. */
    return;
  }
  closeCalendar();
}

/* Whether an element belongs to this output's desktop. Walked rather than
 * measured: the anchor is a bar module, and its bar is a child of the
 * output's own element. */
function isInsideOutput(el, output) {
  for (let node = el; node; node = node.parentElement) {
    if (node === output.el) return true;
  }
  return false;
}
