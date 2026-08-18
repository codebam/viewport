// SPDX-License-Identifier: GPL-3.0-or-later
//
// Stand in for xdg-desktop-portal, so a screen share can be started and
// restored without one.
//
// The frontend is the only thing that ever calls
// org.freedesktop.impl.portal.ScreenCast, and it is also the thing that stores
// the restore data and hands it back on the next run. Both halves are small
// enough to do here, which is what makes the restore path testable headless:
// the alternative is a session bus, a portal frontend, a permission store and
// an application that keeps a token — which is tier three of the same test.
//
// The data is written to a file as D-Bus bytes rather than kept in memory,
// because that is what the permission store does with it, and reading it back
// into a fresh process is the case the compositor has to survive.
//
//   portal-frontend --save token.bin              # share, and write it down
//   portal-frontend --restore token.bin           # share the same thing again
//   portal-frontend --restore token.bin --forge-vendor gnome
//
// Exits 0 when Start answered with a stream, and 1 when it refused.

use std::collections::HashMap;

use zvariant::{ObjectPath, OwnedValue, Value};

/// The connection the compositor serves both portal interfaces on.
const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.viewport";

/// The name the frontend answers on, which is also how the compositor knows a
/// call is the frontend's.
///
/// Every method on the impl interface is checked against this name's owner:
/// the interface is on the session bus, which every process in the session can
/// reach, and a call from anywhere else is something asking to record the desk
/// on its own say-so. A stand-in frontend therefore has to be the frontend,
/// not merely act like one.
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.ScreenCast";

/// What the interface calls a monitor.
const SOURCE_MONITOR: u32 = 1;

/// Persist until explicitly revoked, which is what OBS asks for.
const PERSIST_UNTIL_REVOKED: u32 = 2;

struct Options {
    types: u32,
    persist: u32,
    restore: Option<String>,
    save: Option<String>,
    /// Rewrite the vendor in the restore data before sending it, to see the
    /// compositor refuse a token another desktop wrote.
    forge_vendor: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let options = parse()?;

    let connection = zbus::blocking::Connection::session()?;
    // Claimed before anything is called: the compositor refuses everything
    // from a peer that does not own it.
    connection.request_name(FRONTEND_NAME)?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)?;

    // The paths the frontend would invent per request. Anything unique will
    // do: the compositor keys its sessions by the session path and never
    // looks at the request path.
    let request = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/frontend/1")?;
    let session = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/frontend/1")?;
    let app_id = "org.example.recorder";

    // Retried, because owning the name and the compositor having noticed are
    // two different moments: it follows NameOwnerChanged on its own connection,
    // and a call that overtakes that signal is refused for a reason that has
    // gone away by the time it is read.
    let mut response = 0;
    for _ in 0..100 {
        let (answer, _): (u32, HashMap<String, OwnedValue>) = proxy.call(
            "CreateSession",
            &(
                &request,
                &session,
                app_id,
                HashMap::<String, Value<'_>>::new(),
            ),
        )?;
        response = answer;
        if response == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if response != 0 {
        anyhow::bail!("CreateSession refused with {response}");
    }

    let mut select: HashMap<String, Value<'_>> = HashMap::new();
    select.insert("types".to_owned(), Value::from(options.types));
    select.insert("cursor_mode".to_owned(), Value::from(2u32));
    select.insert("persist_mode".to_owned(), Value::from(options.persist));
    if let Some(path) = options.restore.as_deref() {
        let token = load(path, options.forge_vendor.as_deref())?;
        println!("sent_restore_data=yes");
        select.insert("restore_data".to_owned(), Value::from(token));
    } else {
        println!("sent_restore_data=no");
    }

    let (response, _): (u32, HashMap<String, OwnedValue>) =
        proxy.call("SelectSources", &(&request, &session, app_id, select))?;
    if response != 0 {
        anyhow::bail!("SelectSources refused with {response}");
    }

    let (response, results): (u32, HashMap<String, OwnedValue>) = proxy.call(
        "Start",
        &(
            &request,
            &session,
            app_id,
            "",
            HashMap::<String, Value<'_>>::new(),
        ),
    )?;
    println!("response={response}");
    if response != 0 {
        // Not an error to report loudly: a refusal is what a cancelled
        // chooser looks like, and the test asserts on it in places.
        std::process::exit(1);
    }

    describe(&results);
    if let (Some(path), Some(data)) = (options.save.as_deref(), results.get("restore_data")) {
        save(path, data)?;
        println!("saved={path}");
    }
    Ok(())
}

/// Say what came back, in lines a shell script can grep.
fn describe(results: &HashMap<String, OwnedValue>) {
    // streams is a(ua{sv}) — one entry, because this compositor shares one
    // thing at a time.
    if let Some(Value::Array(streams)) = results.get("streams").map(|value| &**value) {
        for stream in streams.iter() {
            let Value::Structure(stream) = stream else {
                continue;
            };
            let [node, properties] = stream.fields() else {
                continue;
            };
            if let Value::U32(node) = node {
                println!("node={node}");
            }
            let Value::Dict(properties) = properties else {
                continue;
            };
            for (key, value) in properties.iter() {
                let (Value::Str(key), value) = (key, unvariant(value)) else {
                    continue;
                };
                match (key.as_str(), value) {
                    ("size", Value::Structure(size)) => {
                        if let [Value::I32(w), Value::I32(h)] = size.fields() {
                            println!("size={w}x{h}");
                        }
                    }
                    ("source_type", Value::U32(kind)) => println!("source_type={kind}"),
                    _ => {}
                }
            }
        }
    }

    match results.get("persist_mode").map(|value| &**value) {
        Some(Value::U32(mode)) => println!("persist_mode={mode}"),
        _ => println!("persist_mode=absent"),
    }
    println!(
        "got_restore_data={}",
        if results.contains_key("restore_data") {
            "yes"
        } else {
            "no"
        }
    );
}

/// The context the bus itself serialises with.
fn context() -> zvariant::serialized::Context {
    zvariant::serialized::Context::new_dbus(zvariant::Endian::Little, 0)
}

/// Write restore data down the way the permission store does.
fn save(path: &str, data: &OwnedValue) -> anyhow::Result<()> {
    let bytes = zvariant::to_bytes(context(), data)?;
    std::fs::write(path, &*bytes)?;
    Ok(())
}

/// Read it back into a process that did not write it.
fn load(path: &str, forge_vendor: Option<&str>) -> anyhow::Result<OwnedValue> {
    let bytes = std::fs::read(path)?;
    let data = zvariant::serialized::Data::new(bytes, context());
    let (value, _) = data.deserialize::<Value<'_>>()?;

    let Some(vendor) = forge_vendor else {
        return Ok(OwnedValue::try_from(value)?);
    };

    // Somebody else's token, in every respect but the fields inside: the
    // compositor should read the vendor, stop there, and ask.
    let Value::Structure(structure) = value else {
        anyhow::bail!("{path} does not hold a (suv)");
    };
    let mut fields = structure.into_fields();
    if fields.len() != 3 {
        anyhow::bail!("{path} does not hold three fields");
    }
    let inner = fields.pop().expect("three fields");
    let version = fields.pop().expect("three fields");
    Ok(OwnedValue::try_from(Value::from((
        vendor.to_owned(),
        version,
        inner,
    )))?)
}

/// What is inside a variant, however many of them there are.
fn unvariant<'v>(value: &'v Value<'v>) -> &'v Value<'v> {
    let mut value = value;
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

fn parse() -> anyhow::Result<Options> {
    let mut options = Options {
        types: SOURCE_MONITOR,
        persist: PERSIST_UNTIL_REVOKED,
        restore: None,
        save: None,
        forge_vendor: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{arg} wants a value"))
        };
        match arg.as_str() {
            "--types" => options.types = value()?.parse()?,
            "--persist" => options.persist = value()?.parse()?,
            "--restore" => options.restore = Some(value()?),
            "--save" => options.save = Some(value()?),
            "--forge-vendor" => options.forge_vendor = Some(value()?),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(options)
}
