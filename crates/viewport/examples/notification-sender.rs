// SPDX-License-Identifier: MIT
//
// One notification connection that replaces its own message. The integration
// test cannot do this with two `gdbus call` processes: a notification id is
// owned by the D-Bus unique name that created it, and each process has another.

use std::collections::HashMap;

use anyhow::Result;
use zbus::OwnedVariant;

fn main() -> Result<()> {
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
