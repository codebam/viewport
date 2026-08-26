// SPDX-License-Identifier: MIT
//
// One notification connection that replaces its own message. The integration
// test cannot do this with two `gdbus call` processes: a notification id is
// owned by the D-Bus unique name that created it, and each process has another.

use std::collections::HashMap;

use anyhow::Result;
use zvariant::OwnedValue;

fn main() -> Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;

    let first = notify(&proxy, 0, "counting", "1 of 10")?;
    let second = notify(&proxy, first, "counting", "10 of 10")?;
    println!("first={first}");
    println!("second={second}");
    Ok(())
}

fn notify(
    proxy: &zbus::blocking::Proxy<'_>,
    replaces: u32,
    summary: &str,
    body: &str,
) -> Result<u32> {
    Ok(proxy.call(
        "Notify",
        &(
            "viewport-notification-test",
            replaces,
            "",
            summary,
            body,
            Vec::<String>::new(),
            HashMap::<String, OwnedValue>::new(),
            5000i32,
        ),
    )?)
}
