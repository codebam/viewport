// SPDX-License-Identifier: MIT
//
// One notification connection that replaces its own message. The integration
// test cannot do this with two `gdbus call` processes: a notification id is
// owned by the D-Bus unique name that created it, and each process has another.
// The same ownership is why the withdrawal mode exists: with no arguments it
// sends two messages (and prints both ids); with `withdraw <summary>` it sends
// one and closes that same id on the same connection, which is the only way a
// CloseNotification is a withdrawal rather than a stranger's.

use std::collections::HashMap;

use anyhow::{bail, Result};
use zbus::OwnedVariant;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "withdraw" {
        return withdraw(&args[2]);
    }
    if args.len() != 1 {
        bail!("usage: notification-sender [withdraw <summary>]")
    }

    let connection = zbus::block_on(zbus::Connection::session())?;
    let proxy = zbus::block_on(zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    ))?;

    let first = notify(&proxy, 0, "counting", "1 of 10")?;
    let second = notify(&proxy, first, "counting", "10 of 10")?;
    println!("first={first}");
    println!("second={second}");
    Ok(())
}

fn withdraw(summary: &str) -> Result<()> {
    let connection = zbus::block_on(zbus::Connection::session())?;
    let proxy = zbus::block_on(zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    ))?;
    let id = notify(&proxy, 0, summary, "")?;
    println!("id={id}");
    // The close the withdrawal is: the same unique name that created the id
    // ends it. The daemon answers with an empty reply, so `()` is all there is.
    zbus::block_on(proxy.call::<_, _, ()>("CloseNotification", &(id,)))?;
    Ok(())
}

fn notify(proxy: &zbus::Proxy<'_>, replaces: u32, summary: &str, body: &str) -> Result<u32> {
    let reply = zbus::block_on(proxy.call(
        "Notify",
        &(
            "viewport-notification-test",
            replaces,
            "",
            summary,
            body,
            Vec::<String>::new(),
            HashMap::<String, OwnedVariant>::new(),
            5000i32,
        ),
    ))?;
    Ok(reply)
}
