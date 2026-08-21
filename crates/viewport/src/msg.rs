// SPDX-License-Identifier: GPL-3.0-or-later
//
// `viewport msg` — the control socket, from a shell prompt.
//
// The socket is newline-delimited JSON, and every client for it so far has
// been written out by hand: scripts/quit.sh opens a python here-doc to send
// four words. This is the same protocol with the message named rather than
// spelled, in the shape swaymsg uses — the type, then the body:
//
//   viewport msg -t view.focus --id 12
//   viewport msg -t output.query
//   viewport msg -t subscribe view.focused
//
// It is a subcommand of the compositor rather than a binary beside it because
// the two are then the same build by construction. A protocol that grew a
// field cannot be spoken to by a client from the previous release, and a
// second binary is a second thing for every packaging to install and keep in
// step.
//
// Nothing here touches Smithay or a Wayland connection: `main` hands over
// before the compositor is built, so this runs in a session that already has
// one — which is the only session where there is anything to talk to.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// How long to wait for the answer to a query before giving up.
const DEFAULT_TIMEOUT: f64 = 2.0;

/// After a query's first answer, how long a silence means the rest of it has
/// arrived.
///
/// `view.query` replies with a `config` and one `view.added` per window and
/// says nothing to mark the end, so the end is a gap. Long enough to cover the
/// compositor drawing a frame between two of them, short enough not to be felt
/// at a prompt.
const IDLE: Duration = Duration::from_millis(250);

/// Sent behind a message that has no reply, to find out whether it was taken.
///
/// A rejected message comes back as an `error` event to the client that sent
/// it and an accepted one comes back as nothing at all, so the only way to
/// tell the two apart without a clock is to ask a question afterwards and see
/// which arrives first.
///
/// A type the compositor does not know is that question. It is answered at
/// once, on this connection alone, and — being refused before dispatch — it is
/// the only round trip that genuinely runs nothing. `output.query` used to be
/// it and was not inert: it re-announces the whole layout, resizes the shell,
/// re-syncs the shell processes and the wallpaper, and broadcasts an
/// `output.layout` to every subscriber. A script sending a hundred no-reply
/// messages made the compositor do all of that a hundred times.
const BARRIER: &str = "{\"type\":\"viewport.msg.barrier\"}\n";

/// The `context` of the error the barrier comes back as, which is the type
/// name it was sent with (`ParseError::context`). What tells the barrier's own
/// refusal apart from the refusal of the message in front of it.
const BARRIER_CONTEXT: &str = "viewport.msg.barrier";

/// The fields whose values are always a list, so that one `--args right`
/// builds `["right"]` rather than `"right"`.
const ARRAY_FIELDS: &[&str] = &["args", "rects", "workspaces"];

/// The fields the protocol types as a string, which are therefore never read
/// as JSON.
///
/// Without this, `--args 2` is the number 2 and the message is refused, and
/// `--state {}` — a saved session, which is a document inside a string — is an
/// object. Both are things somebody types on the first attempt.
const STRING_FIELDS: &[&str] = &[
    "args",
    "state",
    "name",
    "command",
    "chord",
    "action",
    "transform",
    // The wallpaper's own two. A picture called `2.png` is a name and not a
    // number, and `--mode fill` is a word — while `--mode.width 2560` on an
    // output is matched on its leaf, `width`, so naming this one here does not
    // reach that.
    "path",
    "mode",
    // `sink` and `source`, which are words.
    "target",
    // A tray item's button and the axis its wheel turned, both words.
    "button",
    "orientation",
    // And a media button, which is a word with a hyphen in it.
    "action",
    // A network's name, which is whatever somebody called their router: `5`
    // and `true` are both names a person has given a network, and read as
    // JSON they would be a number and a boolean rather than the network.
    "ssid",
    // And the secret for it, which is a string of anything at all and is the
    // one field here where guessing at its type could change what is sent.
    "passphrase",
    // A Bluetooth address, which is hexadecimal separated by colons.
    "address",
];

struct Type {
    name: &'static str,
    /// Every field this message takes. A parent of nested fields is written
    /// `mode.*`, and matches `--mode.width`.
    fields: &'static [&'static str],
    hint: &'static str,
}

/// Every message the compositor accepts, from `viewport_ipc::Request`.
///
/// Here rather than derived from the enum because it is also the help text and
/// the spelling check: a field the compositor does not read is dropped in
/// silence by serde, so `--visable false` would otherwise send `view.visible`
/// with its default and report success.
const TYPES: &[Type] = &[
    Type {
        name: "view.layout",
        fields: &[
            "id", "x", "y", "width", "height", "scale", "clip.*", "frame.*", "floating", "square",
        ],
        hint: "--id N [--x N --y N --width N --height N] [--scale F] [--floating]",
    },
    Type {
        name: "view.visible",
        fields: &["id", "visible"],
        hint: "--id N [--visible BOOL]",
    },
    Type {
        name: "view.fullscreen",
        fields: &["id", "fullscreen"],
        hint: "--id N [--fullscreen BOOL]",
    },
    Type {
        name: "view.focus",
        fields: &["id"],
        hint: "--id N",
    },
    Type {
        name: "view.close",
        fields: &["id"],
        hint: "--id N",
    },
    Type {
        name: "view.opacity",
        fields: &["id", "opacity"],
        hint: "--id N [--opacity 0..1]",
    },
    Type {
        name: "view.query",
        fields: &[],
        hint: "(replies with config and every window)",
    },
    Type {
        name: "background.focus",
        fields: &[],
        hint: "(toggles focus of the wallpaper terminal)",
    },
    Type {
        name: "shell.focus",
        fields: &[],
        hint: "",
    },
    Type {
        name: "shell.overview",
        fields: &["active"],
        hint: "[--active BOOL]",
    },
    Type {
        name: "shell.overlay",
        fields: &["rects"],
        hint: "--rects JSON",
    },
    Type {
        name: "screencast.rect",
        fields: &["x", "y", "width", "height"],
        hint: "--x N --y N --width N --height N",
    },
    Type {
        name: "session.save",
        fields: &["state"],
        hint: "--state TEXT",
    },
    Type {
        name: "session.query",
        fields: &[],
        hint: "(replies with the saved session)",
    },
    Type {
        name: "notification.action",
        fields: &["id", "action"],
        hint: "--id N [--action KEY]",
    },
    Type {
        name: "notification.dismiss",
        fields: &["id"],
        hint: "--id N",
    },
    Type {
        name: "notification.expire",
        fields: &["id"],
        hint: "--id N",
    },
    Type {
        name: "notification.list",
        fields: &[],
        hint: "(the history, as the shell's centre asks for it)",
    },
    Type {
        name: "notification.forget",
        fields: &["id"],
        hint: "[--id N] (no id forgets everything)",
    },
    Type {
        // The tray's ids are strings — a bus name and an object path joined —
        // rather than the numbers every other `--id` here takes.
        name: "tray.activate",
        fields: &["id", "button", "x", "y"],
        hint: "--id KEY [--button primary|secondary|menu] [--x N --y N]",
    },
    Type {
        name: "clipboard.query",
        fields: &[],
        hint: "(the history, as the shell's picker asks for it)",
    },
    Type {
        name: "clipboard.paste",
        fields: &["id"],
        hint: "--id N",
    },
    Type {
        name: "clipboard.forget",
        fields: &["id"],
        hint: "[--id N] (no id forgets everything)",
    },
    Type {
        name: "launcher.query",
        fields: &["filter"],
        hint: "[--filter TEXT]   (the applications, as the shell's launcher asks for them)",
    },
    Type {
        name: "launcher.launch",
        fields: &["id", "generation"],
        hint: "--id N --generation N   (an id and its generation from the last launcher.list)",
    },
    Type {
        name: "mpris.control",
        fields: &["action"],
        hint: "--action play-pause|next|previous|stop",
    },
    Type {
        name: "power.profile",
        fields: &["profile"],
        hint: "--profile power-saver|balanced|performance",
    },
    Type {
        name: "power.action",
        fields: &["action"],
        hint: "--action suspend|reboot|poweroff   (quit is its own message)",
    },
    Type {
        name: "osk.key",
        fields: &["keysym", "pressed"],
        hint: "--keysym N --pressed BOOL   (an X11/XKB keysym, e.g. 65288 for Backspace)",
    },
    Type {
        name: "network.scan",
        fields: &["enabled"],
        hint: "[--enabled BOOL]   (absent means yes; false closes the picker)",
    },
    Type {
        name: "network.connect",
        fields: &["ssid", "passphrase"],
        hint: "--ssid NAME [--passphrase SECRET]   (no passphrase joins a known network)",
    },
    Type {
        name: "network.disconnect",
        fields: &[],
        hint: "(leave the wireless network in use)",
    },
    Type {
        name: "network.radio",
        fields: &["enabled"],
        hint: "[--enabled BOOL]   (absent toggles)",
    },
    Type {
        name: "bluetooth.power",
        fields: &["enabled"],
        hint: "[--enabled BOOL]   (absent toggles)",
    },
    Type {
        name: "bluetooth.scan",
        fields: &["enabled"],
        hint: "[--enabled BOOL]   (absent means yes; false closes the picker)",
    },
    Type {
        name: "bluetooth.device",
        fields: &["address", "action"],
        hint: "--address AA:BB:CC:DD:EE:FF --action pair|connect|disconnect|trust|untrust|forget",
    },
    Type {
        name: "tray.menu.click",
        fields: &["id", "item"],
        hint: "--id KEY --item N",
    },
    Type {
        name: "tray.menu.closed",
        fields: &["id"],
        hint: "--id KEY",
    },
    Type {
        name: "tray.scroll",
        fields: &["id", "delta", "orientation"],
        hint: "--id KEY --delta N [--orientation vertical|horizontal]",
    },
    Type {
        name: "workspace.list",
        fields: &["workspaces"],
        hint: "--workspaces JSON",
    },
    Type {
        name: "output.configure",
        fields: &[
            "name",
            "enabled",
            "mode.*",
            "scale",
            "transform",
            "adaptive_sync",
            "x",
            "y",
        ],
        hint: "--name NAME [--enabled BOOL] [--mode.width N --mode.height N --mode.refresh N] [--scale F] [--transform T] [--x N --y N]",
    },
    Type {
        name: "output.hdr",
        fields: &["name", "enabled"],
        hint: "[--name NAME] [--enabled BOOL]   (absent --enabled toggles)",
    },
    Type {
        name: "output.confirm",
        fields: &[],
        hint: "",
    },
    Type {
        name: "output.active",
        fields: &["name"],
        hint: "--name NAME",
    },
    Type {
        name: "output.query",
        fields: &[],
        hint: "(replies with the output layout)",
    },
    Type {
        name: "output.test_add",
        fields: &[],
        hint: "(headless only)",
    },
    Type {
        name: "output.test_remove",
        fields: &["name"],
        hint: "[--name NAME]   (headless only)",
    },
    Type {
        name: "bind.add",
        fields: &["chord", "action"],
        hint: "--chord Mod4+a --action focus.parent",
    },
    Type {
        name: "config.gaps",
        fields: &["inner", "outer", "smart"],
        hint: "[--inner N --outer N --smart BOOL]   (set window gaps; each is optional)",
    },
    Type {
        name: "config.border",
        fields: &["radius", "width", "smart"],
        hint: "[--radius N --width N --smart BOOL]   (set the window border; each is optional)",
    },
    Type {
        name: "config.wallpaper",
        fields: &["path", "mode"],
        hint: "[--path FILE] [--mode fill|fit|stretch|center|tile]   (--path '' removes it)",
    },
    Type {
        name: "shell.command",
        fields: &["command", "args"],
        hint: "--command VERB [--args ARG]...",
    },
    Type {
        name: "shell.exec",
        fields: &["command"],
        hint: "--command LINE   (run a shell command on the host)",
    },
    Type {
        name: "status.refresh",
        fields: &[],
        hint: "(re-sample the status bar now, not on the next tick)",
    },
    Type {
        name: "status.volume",
        fields: &["target", "delta", "mute"],
        hint: "--target sink|source [--delta 5] [--mute]   (changes it, then re-samples)",
    },
    Type {
        name: "input.pointer",
        fields: &["x", "y"],
        hint: "--x F --y F",
    },
    Type {
        name: "input.button",
        fields: &["button", "pressed"],
        hint: "--button 272 [--pressed BOOL]",
    },
    Type {
        name: "input.key",
        fields: &["keycode", "pressed"],
        hint: "--keycode 34 [--pressed BOOL]",
    },
    Type {
        name: "quit",
        fields: &[],
        hint: "(stops the compositor)",
    },
];

const USAGE: &str = "\
usage: viewport msg [options] -t TYPE [--field value]...
       viewport msg [options] -t subscribe [event]...

Speaks the control socket of a running compositor: one JSON message, then
whatever it answers with.

Options:
  -t, --type TYPE     the message to send, or `subscribe` to watch events
      --socket PATH   the socket to use. Otherwise $VIEWPORT_SOCKET, then
                      $XDG_RUNTIME_DIR/viewport-$WAYLAND_DISPLAY.sock, then
                      the newest viewport-*.sock there
      --timeout SECS  how long to wait for a reply (default 2; 0 waits)
      --pretty        indent the JSON that comes back
      --raw JSON      send this object verbatim, instead of -t and its fields
  -h, --help          this
";

const EXAMPLES: &str = "\
Examples:
  viewport msg -t view.focus --id 12
  viewport msg -t view.query --pretty
  viewport msg -t shell.command --command output.focus --args right
  viewport msg -t output.configure --name DP-1 --mode.width 2560 --mode.height 1440
  viewport msg -t subscribe view.focused view.removed
  viewport msg -t quit

Values are read as JSON where they parse as one and as text where they do not,
so --id 12 is a number, --name DP-1 is a string, and --enabled false is a
boolean. The fields the protocol types as text stay text whatever they look
like. A flag with no value is true: --floating.
";

/// `viewport msg`, from `main`. Returns the process's exit status.
pub fn main(args: &[String]) -> i32 {
    let invocation = match parse_args(args) {
        Ok(Some(invocation)) => invocation,
        // `--help`, which is not a failure.
        Ok(None) => return 0,
        Err(complaint) => {
            eprintln!("viewport msg: {complaint}");
            eprintln!("try `viewport msg --help`");
            return 2;
        }
    };

    match run(invocation) {
        Ok(()) => 0,
        Err(complaint) => {
            eprintln!("viewport msg: {complaint}");
            1
        }
    }
}

struct Invocation {
    /// The message to send, already valid. `None` for `subscribe`, which sends
    /// nothing.
    message: Option<String>,
    /// The event types to print, empty for all of them. Only for `subscribe`.
    watching: Vec<String>,
    reply: Reply,
    socket: Option<PathBuf>,
    timeout: Option<Duration>,
    pretty: bool,
}

/// What to wait for once the message has gone out.
enum Reply {
    /// Nothing answers this one; a refusal is read out from behind a
    /// [`BARRIER`].
    None,
    /// One event of this type, and then done.
    First(&'static str),
    /// Every event of these types, until a silence says there are no more.
    Until(&'static [&'static str]),
    /// Everything, forever.
    Stream,
}

/// What a given message is answered with (`crate::apply`).
fn reply_for(kind: &str) -> Reply {
    match kind {
        "output.query" => Reply::First("output.layout"),
        "session.query" => Reply::First("session.restore"),
        "view.query" => Reply::Until(&["config", "view.added"]),
        "launcher.query" => Reply::First("launcher.list"),
        _ => Reply::None,
    }
}

fn parse_args(args: &[String]) -> Result<Option<Invocation>, String> {
    let mut kind: Option<String> = None;
    let mut raw: Option<String> = None;
    let mut socket: Option<PathBuf> = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut pretty = false;
    // Named and typed but not yet assembled: which fields a message may have
    // depends on which message it is, and `-t` can come after them.
    let mut given: Vec<(String, Value)> = Vec::new();
    let mut positional: Vec<String> = Vec::new();

    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();
        at += 1;

        if arg == "-h" || arg == "--help" {
            print_help();
            return Ok(None);
        }

        // `--name=value` and `--name value` both, because both are what people
        // type and the compositor's own flags take both.
        let (name, joined) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg, None),
        };

        // The value of the flag being read, from `=` or from the next
        // argument. A flag at the end of the line, or one followed by another
        // flag, has none — which for a body field means `true` and for our own
        // options is a mistake.
        let mut value = || -> Option<String> {
            if let Some(joined) = joined.clone() {
                return Some(joined);
            }
            let next = args.get(at)?;
            if next.starts_with("--") {
                return None;
            }
            at += 1;
            Some(next.clone())
        };

        match name {
            "-t" | "--type" => {
                let given = value().ok_or("-t needs a message type")?;
                if kind.replace(given).is_some() {
                    return Err("-t was given twice".into());
                }
            }
            "--socket" => {
                socket = Some(PathBuf::from(value().ok_or("--socket needs a path")?));
            }
            "--timeout" => {
                let given = value().ok_or("--timeout needs a number of seconds")?;
                timeout = given
                    .parse::<f64>()
                    .map_err(|_| format!("--timeout is not a number: {given}"))?;
                // The upper bound is not pedantry: `Duration::from_secs_f64`
                // panics past `Duration::MAX`, and `1e30` parses as happily
                // as `2` does. Same complaint as the other two, because it
                // is the same mistake — a number that is not a length of
                // time.
                if timeout < 0.0 || !timeout.is_finite() || timeout > Duration::MAX.as_secs_f64() {
                    return Err(format!("--timeout is not a length of time: {given}"));
                }
            }
            "--pretty" => pretty = true,
            "--raw" => {
                raw = Some(value().ok_or("--raw needs a JSON object")?);
            }
            _ if name.starts_with("--") => {
                let field = &name[2..];
                if field.is_empty() {
                    return Err("`--` is not a field".into());
                }
                let literal = match value() {
                    Some(text) => literal(field, &text),
                    // `--floating`, with nothing after it.
                    None => Value::Bool(true),
                };
                given.push((field.to_owned(), literal));
            }
            _ if name.starts_with('-') && name.len() > 1 => {
                return Err(format!("unknown option {name}"));
            }
            _ => positional.push(arg.to_owned()),
        }
    }

    let timeout = (timeout > 0.0).then(|| Duration::from_secs_f64(timeout));

    if let Some(raw) = raw {
        if kind.is_some() || !given.is_empty() {
            return Err("--raw is the whole message; -t and fields cannot be given with it".into());
        }
        let message: Value = serde_json::from_str(&raw).map_err(|e| format!("--raw: {e}"))?;
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .ok_or("--raw: the object needs a string `type`")?
            .to_owned();
        // Through the compositor's own parser, so a message it would refuse is
        // refused here instead — where the complaint arrives on the terminal
        // that typed it.
        viewport_ipc::parse(raw.as_bytes()).map_err(|e| e.to_string())?;
        return Ok(Some(Invocation {
            message: Some(raw),
            watching: Vec::new(),
            reply: reply_for(&kind),
            socket,
            timeout,
            pretty,
        }));
    }

    let kind = match kind {
        Some(kind) => kind,
        // A message named without `-t` is what anyone who knows the protocol
        // and not this program types first.
        None => {
            let named = positional
                .first()
                .filter(|word| {
                    *word == "subscribe" || TYPES.iter().any(|entry| entry.name == **word)
                })
                .map(|word| format!(": did you mean `-t {word}`?"))
                .unwrap_or_default();
            return Err(format!("no message: -t is what says which one{named}"));
        }
    };

    if kind == "subscribe" {
        if !given.is_empty() {
            return Err("subscribe takes event names, not fields".into());
        }
        return Ok(Some(Invocation {
            message: None,
            watching: positional,
            reply: Reply::Stream,
            socket,
            timeout,
            pretty,
        }));
    }

    if let Some(unexpected) = positional.first() {
        return Err(format!(
            "unexpected argument `{unexpected}`: fields are named, as --{unexpected}"
        ));
    }

    let Some(entry) = TYPES.iter().find(|entry| entry.name == kind) else {
        return Err(format!(
            "unknown message type `{kind}`. `viewport msg --help` lists them"
        ));
    };

    // Against the names as they were typed, not against the object they build:
    // `--mode.width` is a field of this message and `mode` is not a name
    // anybody wrote.
    let mut body = Map::new();
    for (field, value) in given {
        let known = entry
            .fields
            .iter()
            .any(|allowed| match allowed.strip_suffix('*') {
                Some(prefix) => field.starts_with(prefix),
                None => *allowed == field,
            });
        if !known {
            return Err(match entry.fields {
                [] => format!("{kind} takes no fields, and was given --{field}"),
                fields => format!(
                    "{kind} has no field `{field}`. It takes: {}",
                    fields.join(", ")
                ),
            });
        }
        insert(&mut body, &field, value)?;
    }

    body.insert("type".into(), Value::String(kind.clone()));
    let message = Value::Object(body).to_string();
    viewport_ipc::parse(message.as_bytes()).map_err(|e| e.to_string())?;

    Ok(Some(Invocation {
        message: Some(message),
        watching: Vec::new(),
        reply: reply_for(&kind),
        socket,
        timeout,
        pretty,
    }))
}

/// A field's value: JSON where it is JSON, text where it is not.
///
/// `--id 12` is the number, `--name DP-1` is the string, `--enabled false` is
/// the boolean — which is what the protocol wants in each case and what anyone
/// would write. The fields the protocol types as a string are exempt, so that
/// an output called `2` stays its name and a saved session stays a document.
fn literal(field: &str, text: &str) -> Value {
    let leaf = field.rsplit('.').next().unwrap_or(field);
    if STRING_FIELDS.contains(&leaf) {
        return Value::String(text.to_owned());
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// Put one `--field` into the message, building the objects a dotted name
/// implies.
fn insert(body: &mut Map<String, Value>, field: &str, value: Value) -> Result<(), String> {
    let mut object = body;
    let mut segments = field.split('.').peekable();

    while let Some(segment) = segments.next() {
        if segments.peek().is_some() {
            let nested = object
                .entry(segment.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            object = nested
                .as_object_mut()
                .ok_or_else(|| format!("--{field} and --{segment} cannot both be given"))?;
            continue;
        }

        // A list field collects, so `--args a --args b` is both of them and
        // `--args a` alone is still a list.
        if ARRAY_FIELDS.contains(&segment) {
            match object
                .entry(segment.to_owned())
                .or_insert_with(|| Value::Array(Vec::new()))
            {
                // A value that is already a list is the list, not one item of
                // one: `--rects '[{...}]'` is the documented spelling, and
                // wrapping it again sent `[[{...}]]` — a message the compositor
                // refuses, from the line its own help tells the user to type.
                Value::Array(items) => match value {
                    Value::Array(given) => items.extend(given),
                    value => items.push(value),
                },
                _ => return Err(format!("--{field} was already given as something else")),
            }
            return Ok(());
        }

        if object.insert(segment.to_owned(), value).is_some() {
            return Err(format!("--{field} was given twice"));
        }
        return Ok(());
    }

    Ok(())
}

fn print_help() {
    print!("{USAGE}");
    println!();
    println!("Message types:");
    for entry in TYPES {
        if entry.hint.is_empty() {
            println!("  {}", entry.name);
        } else {
            println!("  {:<20} {}", entry.name, entry.hint);
        }
    }
    println!();
    print!("{EXAMPLES}");
}

fn run(invocation: Invocation) -> Result<(), String> {
    let path = match invocation.socket {
        Some(path) => path,
        None => discover()?,
    };

    let stream = UnixStream::connect(&path)
        .map_err(|e| format!("could not connect to {}: {e}", path.display()))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("could not read from {}: {e}", path.display()))?,
    );

    if let Some(message) = &invocation.message {
        (&stream)
            .write_all(format!("{message}\n").as_bytes())
            .map_err(|e| format!("could not send to {}: {e}", path.display()))?;
    }

    // A duration the clock cannot hold is no deadline at all: `Instant` has a
    // range of its own, and adding a timeout past it panicked where the
    // validation above never reaches. A wait that long is a way of saying
    // "forever", and forever already has a spelling here.
    let deadline = invocation
        .timeout
        .and_then(|timeout| Instant::now().checked_add(timeout));
    let pretty = invocation.pretty;
    // The line being read, which outlives any one read: a timeout can land in
    // the middle of one. See `read_event`.
    let mut partial = String::new();

    match invocation.reply {
        Reply::Stream => {
            // No deadline: watching is the whole job, and it ends when the
            // compositor does or the terminal is closed.
            stream.set_read_timeout(None).ok();
            loop {
                match read_event(&mut reader, &mut partial)? {
                    Incoming::Event(event) => {
                        let kind = kind_of(&event);
                        if invocation.watching.is_empty()
                            || invocation.watching.iter().any(|want| want == kind)
                        {
                            print_event(&event, pretty);
                        }
                    }
                    Incoming::Timeout => continue,
                    Incoming::Eof => return Ok(()),
                }
            }
        }

        Reply::None => {
            // Nothing comes back but a refusal, and waiting a while for one
            // that may never come is a guess: too short and a busy compositor
            // reports success for a message it threw away, too long and every
            // send costs the wait. So the wait is for something that does
            // answer, sent behind it.
            //
            // The compositor reads and dispatches a connection's messages in
            // the order they arrive, and answers them on the same buffer, so
            // the barrier's own refusal here is proof that the message before
            // it has already been handled — and an `error` read on the way to
            // it is that message's own.
            let _ = (&stream).write_all(BARRIER.as_bytes());
            loop {
                if !arm(&stream, deadline.unwrap_or_else(forever))? {
                    return Ok(());
                }
                match read_event(&mut reader, &mut partial) {
                    Ok(Incoming::Event(event)) => match kind_of(&event) {
                        "error" if context_of(&event) == BARRIER_CONTEXT => return Ok(()),
                        "error" => return Err(complaint(&event)),
                        _ => continue,
                    },
                    // The compositor going away is what `quit` asked for,
                    // and a silence after it is not a failure either. A read
                    // error is: the conversation ended mid-word, and
                    // reporting success for it would be the one lie this
                    // tool cannot tell.
                    Ok(Incoming::Eof) | Ok(Incoming::Timeout) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        }

        Reply::First(wanted) => loop {
            if !arm(&stream, deadline.unwrap_or_else(forever))? {
                return Err(format!("nothing answered with {wanted} in time"));
            }
            match read_event(&mut reader, &mut partial)? {
                Incoming::Event(event) => match kind_of(&event) {
                    kind if kind == wanted => {
                        print_event(&event, pretty);
                        return Ok(());
                    }
                    "error" => return Err(complaint(&event)),
                    _ => continue,
                },
                Incoming::Timeout => return Err(format!("nothing answered with {wanted} in time")),
                Incoming::Eof => return Err("the compositor closed the connection".into()),
            }
        },

        Reply::Until(wanted) => {
            let mut seen = false;
            let ran_out = || format!("nothing answered with {} in time", wanted.join(" or "));
            loop {
                // Before the first answer the wait is the timeout; after it, a
                // gap is the end of the answer — there is no message that
                // marks one.
                let until = match (seen, deadline) {
                    (true, _) => Instant::now() + IDLE,
                    (false, Some(deadline)) => deadline,
                    (false, None) => forever(),
                };
                if !arm(&stream, until)? {
                    return if seen { Ok(()) } else { Err(ran_out()) };
                }
                match read_event(&mut reader, &mut partial)? {
                    Incoming::Event(event) => match kind_of(&event) {
                        "error" => return Err(complaint(&event)),
                        kind if wanted.contains(&kind) => {
                            seen = true;
                            print_event(&event, pretty);
                        }
                        _ => continue,
                    },
                    Incoming::Timeout | Incoming::Eof if seen => return Ok(()),
                    Incoming::Timeout => return Err(ran_out()),
                    Incoming::Eof => return Err("the compositor closed the connection".into()),
                }
            }
        }
    }
}

/// Long enough to be no limit at all, for `--timeout 0`. A duration rather
/// than an `Option` threaded through every branch.
fn forever() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}

/// Set the socket's read timeout so the next read stops at `until`. `false`
/// means it has already passed.
///
/// The socket's own timeout rather than poll(2), because the reader is
/// buffered: a line already in the buffer is returned without a syscall, and
/// this only decides how long the read that refills it may block.
fn arm(stream: &UnixStream, until: Instant) -> Result<bool, String> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(false);
    }
    stream
        .set_read_timeout(Some(left))
        .map_err(|e| format!("could not set a read timeout: {e}"))?;
    Ok(true)
}

#[derive(Debug)]
enum Incoming {
    Event(Value),
    /// The read timeout arrived first. What that means is the caller's: for a
    /// query it is a failure, for the tail of one it is the end.
    Timeout,
    /// The compositor closed the socket, or stopped.
    Eof,
}

/// Read one event, keeping whatever a timeout interrupted.
///
/// `partial` is the caller's, and is why: `read_line` consumes as it reads, so
/// a read timeout that lands mid-line has already taken those bytes out of the
/// socket. Dropping the string here threw them away, and the next read then
/// began in the middle of a message — an `output.layout` split across two
/// reads came back truncated, or made the read after it unparseable. So the
/// half line stays in the caller's buffer and the next read finishes it.
fn read_event(
    reader: &mut BufReader<UnixStream>,
    partial: &mut String,
) -> Result<Incoming, String> {
    loop {
        match reader.read_line(partial) {
            Ok(0) => return Ok(Incoming::Eof),
            Ok(_) => {
                let line = std::mem::take(partial);
                let line = line.trim();
                // The compositor writes one message per line, but a blank line
                // is ignored rather than refused everywhere else in this
                // protocol and is ignored here too.
                if line.is_empty() {
                    continue;
                }
                return serde_json::from_str(line)
                    .map(Incoming::Event)
                    .map_err(|e| format!("the compositor sent something unreadable: {e}"));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(Incoming::Timeout)
            }
            Err(e) => return Err(format!("could not read from the socket: {e}")),
        }
    }
}

fn kind_of(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or("")
}

/// What an `error` event was refused against: the message's own type, or
/// `ipc` for anything that failed before there was a type to name.
fn context_of(event: &Value) -> &str {
    event
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("ipc")
}

/// An `error` event, as the line to print.
fn complaint(event: &Value) -> String {
    let context = context_of(event);
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("refused, with no reason given");
    format!("{context}: {message}")
}

fn print_event(event: &Value, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(event)
    } else {
        serde_json::to_string(event)
    };
    if let Ok(text) = text {
        println!("{text}");
        // A line at a time: `subscribe` is watched live, and a pipe buffers by
        // the block otherwise — which shows nothing until something goes
        // wrong and the buffer is lost.
        let _ = std::io::stdout().flush();
    }
}

/// Which compositor to talk to, when nobody said.
///
/// `$VIEWPORT_SOCKET` first: a compositor exports it, so anything started from
/// a session is already pointed at that session. Then the name built from the
/// Wayland display, which is what a terminal in the session has. Then the
/// newest socket in the runtime directory, which is what a second TTY has —
/// the same last resort scripts/quit.sh takes, and for the same reason: the
/// session being escaped from is the one that was started last.
fn discover() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("VIEWPORT_SOCKET") {
        return Ok(PathBuf::from(path));
    }

    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());

    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        let path = PathBuf::from(format!("{runtime}/viewport-{display}.sock"));
        if path.exists() {
            return Ok(path);
        }
    }

    if let Some(path) = newest_socket(Path::new(&runtime)) {
        return Ok(path);
    }

    Err(format!(
        "no compositor found: nothing set $VIEWPORT_SOCKET and there is no \
         viewport-*.sock in {runtime}. Name one with --socket"
    ))
}

fn newest_socket(runtime: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(runtime).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_str()?;
        if !name.starts_with("viewport-") || !name.ends_with(".sock") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|data| data.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(newest, _)| modified > *newest) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let invocation = parse_args(&args)?.expect("not --help");
        Ok(invocation.message.unwrap_or_default())
    }

    fn value(args: &[&str]) -> Value {
        serde_json::from_str(&build(args).expect("should build")).expect("should be JSON")
    }

    #[test]
    fn a_type_and_its_fields_become_the_message() {
        assert_eq!(
            value(&["-t", "view.focus", "--id", "12"]),
            serde_json::json!({"type": "view.focus", "id": 12})
        );
    }

    #[test]
    fn config_gaps_builds_a_number_not_a_string() {
        assert_eq!(
            value(&["-t", "config.gaps", "--inner", "15"]),
            serde_json::json!({"type": "config.gaps", "inner": 15})
        );
        assert_eq!(
            value(&[
                "-t",
                "config.gaps",
                "--inner",
                "15",
                "--outer",
                "4",
                "--smart",
                "true"
            ]),
            serde_json::json!({
                "type": "config.gaps", "inner": 15, "outer": 4, "smart": true
            })
        );
    }

    #[test]
    fn config_border_builds_a_number_not_a_string() {
        assert_eq!(
            value(&[
                "-t",
                "config.border",
                "--radius",
                "12",
                "--width",
                "3",
                "--smart",
                "true"
            ]),
            serde_json::json!({
                "type": "config.border", "radius": 12, "width": 3, "smart": true
            })
        );
    }

    #[test]
    fn values_are_json_where_they_are_json_and_text_where_they_are_not() {
        assert_eq!(
            value(&["-t", "output.hdr", "--name", "DP-1", "--enabled", "false"]),
            serde_json::json!({"type": "output.hdr", "name": "DP-1", "enabled": false})
        );
    }

    #[test]
    fn a_flag_with_nothing_after_it_is_true() {
        assert_eq!(
            value(&["-t", "view.fullscreen", "--id", "3", "--fullscreen"]),
            serde_json::json!({"type": "view.fullscreen", "id": 3, "fullscreen": true})
        );
    }

    #[test]
    fn joined_and_separated_flags_are_the_same_flag() {
        assert_eq!(
            value(&["-t=view.focus", "--id=12"]),
            value(&["-t", "view.focus", "--id", "12"])
        );
    }

    #[test]
    fn a_negative_number_is_a_value_and_not_an_option() {
        // The shell sends -1 for "no view", and so does anyone unfocusing by
        // hand.
        assert_eq!(
            value(&["-t", "view.focus", "--id", "-1"]),
            serde_json::json!({"type": "view.focus", "id": -1})
        );
    }

    #[test]
    fn a_dotted_field_builds_the_object_it_names() {
        assert_eq!(
            value(&[
                "-t",
                "output.configure",
                "--name",
                "DP-1",
                "--mode.width",
                "2560",
                "--mode.height",
                "1440",
            ]),
            serde_json::json!({
                "type": "output.configure",
                "name": "DP-1",
                "mode": {"width": 2560, "height": 1440},
            })
        );
    }

    #[test]
    fn a_list_field_is_a_list_with_one_value_in_it() {
        assert_eq!(
            value(&[
                "-t",
                "shell.command",
                "--command",
                "output.focus",
                "--args",
                "right"
            ]),
            serde_json::json!({
                "type": "shell.command",
                "command": "output.focus",
                "args": ["right"],
            })
        );
    }

    #[test]
    fn a_list_field_collects() {
        assert_eq!(
            value(&[
                "-t",
                "shell.command",
                "--command",
                "view.move",
                "--args",
                "left",
                "--args",
                "2",
            ]),
            serde_json::json!({
                "type": "shell.command",
                "command": "view.move",
                // Text, not the number 2: `args` is a list of strings, and a
                // number there is a message the compositor refuses.
                "args": ["left", "2"],
            })
        );
    }

    #[test]
    fn a_list_given_as_a_list_is_not_wrapped_in_another_one() {
        // The documented spelling, and the one the help text asks for:
        // `--rects '[{...}]'` is the whole list. Pushed whole it became
        // `[[{...}]]`, which the compositor refuses — so the line printed in
        // the hint exited 2.
        assert_eq!(
            value(&[
                "-t",
                "shell.overlay",
                "--rects",
                r#"[{"x":0,"y":0,"width":10,"height":10}]"#,
            ]),
            serde_json::json!({
                "type": "shell.overlay",
                "rects": [{"x": 0, "y": 0, "width": 10, "height": 10}],
            })
        );
    }

    #[test]
    fn a_list_given_as_a_list_still_collects() {
        // Two of them concatenate rather than nest, which is the same rule the
        // repeated `--args` follows.
        assert_eq!(
            value(&[
                "-t",
                "shell.overlay",
                "--rects",
                r#"[{"x":0,"y":0,"width":10,"height":10}]"#,
                "--rects",
                r#"{"x":1,"y":1,"width":2,"height":2}"#,
            ]),
            serde_json::json!({
                "type": "shell.overlay",
                "rects": [
                    {"x": 0, "y": 0, "width": 10, "height": 10},
                    {"x": 1, "y": 1, "width": 2, "height": 2},
                ],
            })
        );
    }

    #[test]
    fn a_verb_that_takes_no_arguments_needs_no_args() {
        assert_eq!(
            value(&["-t", "shell.command", "--command", "layout.overview"]),
            serde_json::json!({"type": "shell.command", "command": "layout.overview"})
        );
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_dropped() {
        // serde ignores what it does not recognise, so without this check
        // `--visable false` would send view.visible with its default — which
        // is `true`, the opposite of what was asked for, reported as success.
        let complaint = build(&["-t", "view.visible", "--id", "1", "--visable", "false"])
            .expect_err("should be refused");
        assert!(complaint.contains("has no field `visable`"), "{complaint}");
        assert!(complaint.contains("visible"), "{complaint}");
    }

    #[test]
    fn a_timeout_past_what_a_duration_holds_is_refused_not_a_panic() {
        // `Duration::from_secs_f64` panics above `Duration::MAX`, and `1e30`
        // parses as happily as `2` does.
        let complaint = build(&["-t", "quit", "--timeout", "1e30"]).expect_err("should refuse");
        assert!(complaint.contains("not a length of time"), "{complaint}");
        // One the duration holds but the clock may not is still accepted
        // here; `run` treats a deadline the clock cannot represent as no
        // deadline at all, which is what a wait that long means.
        assert!(build(&["-t", "quit", "--timeout", "1000000000000000000"]).is_ok());
    }

    #[test]
    fn a_field_on_a_message_that_takes_none_is_refused() {
        let complaint = build(&["-t", "quit", "--now"]).expect_err("should be refused");
        assert!(complaint.contains("takes no fields"), "{complaint}");
    }

    #[test]
    fn an_unknown_type_is_refused_before_anything_is_sent() {
        let complaint = build(&["-t", "view.teleport"]).expect_err("should be refused");
        assert!(complaint.contains("unknown message type"), "{complaint}");
    }

    #[test]
    fn a_missing_field_is_refused_with_the_compositors_own_complaint() {
        let complaint = build(&["-t", "view.focus"]).expect_err("should be refused");
        assert!(complaint.contains("view.focus"), "{complaint}");
        assert!(complaint.contains("id"), "{complaint}");
    }

    #[test]
    fn a_field_given_twice_is_a_mistake_and_not_the_last_one() {
        let complaint =
            build(&["-t", "view.focus", "--id", "1", "--id", "2"]).expect_err("should be refused");
        assert!(complaint.contains("twice"), "{complaint}");
    }

    #[test]
    fn no_type_is_a_usage_error() {
        let complaint = build(&["--id", "12"]).expect_err("should be refused");
        assert!(complaint.contains("-t"), "{complaint}");
    }

    #[test]
    fn raw_is_sent_as_written_and_still_checked() {
        assert_eq!(
            value(&["--raw", r#"{"type":"view.opacity","id":3,"opacity":0.5}"#]),
            serde_json::json!({"type": "view.opacity", "id": 3, "opacity": 0.5})
        );
        let complaint =
            build(&["--raw", r#"{"type":"view.focus","id":"seven"}"#]).expect_err("should refuse");
        assert!(complaint.contains("view.focus"), "{complaint}");
    }

    #[test]
    fn subscribe_sends_nothing_and_watches_what_it_was_given() {
        let args: Vec<String> = ["-t", "subscribe", "view.focused", "view.removed"]
            .iter()
            .map(|a| (*a).to_owned())
            .collect();
        let invocation = parse_args(&args).expect("should parse").expect("not help");
        assert!(invocation.message.is_none());
        assert_eq!(invocation.watching, ["view.focused", "view.removed"]);
        assert!(matches!(invocation.reply, Reply::Stream));
    }

    #[test]
    fn a_word_where_a_field_belongs_says_so() {
        let complaint = build(&["-t", "view.focus", "12"]).expect_err("should be refused");
        assert!(complaint.contains("--12"), "{complaint}");
    }

    #[test]
    fn every_type_offered_is_a_type_the_compositor_knows() {
        // A renamed request fails here rather than at a prompt, and the help
        // text cannot drift from the protocol without this noticing.
        for entry in TYPES {
            let message = format!(r#"{{"type":"{}"}}"#, entry.name);
            match viewport_ipc::parse(message.as_bytes()) {
                // Missing fields are expected: this is only asking whether the
                // name is one the compositor dispatches.
                Ok(_) | Err(viewport_ipc::ParseError::BadBody { .. }) => {}
                Err(e) => panic!("{}: {e}", entry.name),
            }
        }
    }

    #[test]
    fn the_offered_types_are_the_whole_request_set() {
        // `viewport_ipc::Request` has 59 variants. A new one that is not listed
        // here cannot be sent, and the only place that would show up is a
        // prompt saying it is unknown.
        assert_eq!(TYPES.len(), 59);
    }

    #[test]
    fn a_read_timeout_does_not_eat_the_line_it_landed_in() {
        // `read_line` consumes as it goes, so the bytes of a half-arrived
        // message are already out of the socket when the timeout fires.
        // Dropping them truncated a long `output.layout` and left the read
        // after it looking at the tail of a message it never saw the head of.
        let (mine, theirs) = UnixStream::pair().expect("a socket pair");
        mine.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("a read timeout");
        let mut reader = BufReader::new(mine);
        let mut partial = String::new();

        (&theirs)
            .write_all(br#"{"type":"output.la"#)
            .expect("the first half");
        assert!(matches!(
            read_event(&mut reader, &mut partial),
            Ok(Incoming::Timeout)
        ));

        (&theirs).write_all(b"yout\"}\n").expect("the second half");
        let event = match read_event(&mut reader, &mut partial) {
            Ok(Incoming::Event(event)) => event,
            other => panic!("the rest of the line should have completed it: {other:?}"),
        };
        assert_eq!(kind_of(&event), "output.layout");
    }

    #[test]
    fn the_barrier_is_a_message_the_compositor_runs_nothing_for() {
        // It has to be refused — that refusal is the round trip — and the
        // refusal has to name it, because that is what tells the barrier's own
        // error apart from the error of the message in front of it. A build
        // that grew a request by this name would break both.
        match viewport_ipc::parse(BARRIER.trim().as_bytes()) {
            Err(viewport_ipc::ParseError::UnknownType { name }) => {
                assert_eq!(name, BARRIER_CONTEXT);
            }
            other => panic!("the barrier must be refused before dispatch: {other:?}"),
        }
    }

    #[test]
    fn a_query_waits_for_the_event_that_answers_it() {
        assert!(matches!(
            reply_for("output.query"),
            Reply::First("output.layout")
        ));
        assert!(matches!(reply_for("view.query"), Reply::Until(_)));
        assert!(matches!(reply_for("view.focus"), Reply::None));
    }
}
