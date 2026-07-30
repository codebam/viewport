// SPDX-License-Identifier: GPL-3.0-or-later
//
// The headless backend. Ports the --headless half of src/output.c.
//
// No renderer and no window: virtual outputs with a fixed mode, and a timer
// standing in for vblank. Clients connect, map, and get frame callbacks, so
// every part of the window lifecycle and the whole IPC protocol can be driven
// without a GPU or a display — which is what makes the compositor testable in
// CI, and what `output.test_add` exists for.

use std::collections::HashMap;
use std::time::Duration;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::Transform;

use crate::state::ViewportState;

/// The virtual refresh rate, in mHz. Frame callbacks fire at this rate.
const REFRESH: i32 = 60_000;

/// The virtual outputs, and what it takes to make another one.
pub struct Headless {
    /// Every virtual output has the same mode, including the ones plugged in
    /// later: `output.test_add` is about *how many* monitors there are and
    /// where they sit, and a second size would only be a second variable in
    /// the tests that use it.
    mode: Mode,
    /// The global each output owns, so unplugging can take it away. Dropping a
    /// `GlobalId` does not remove the global — `DisplayHandle::remove_global`
    /// does, and it needs the id back.
    globals: HashMap<String, GlobalId>,
    /// The number the next output gets. Only ever counts up: a name that comes
    /// back after an unplug is a different monitor to anything holding the old
    /// one, and reusing it is how a stale reference starts looking valid.
    next: u32,
}

pub fn init(
    event_loop: &mut EventLoop<'static, ViewportState>,
    state: &mut ViewportState,
    width: i32,
    height: i32,
) -> anyhow::Result<()> {
    state.headless = Some(Headless {
        mode: Mode {
            size: (width, height).into(),
            refresh: REFRESH,
        },
        globals: HashMap::new(),
        next: 1,
    });

    let name = add(state).ok_or_else(|| anyhow::anyhow!("creating the first headless output"))?;
    state.active_output = Some(name);

    let frame_interval = Duration::from_micros(1_000_000_000 / REFRESH as u64);

    event_loop
        .handle()
        .insert_source(Timer::from_duration(frame_interval), move |_, _, state| {
            // A client will not paint frame N+1 until frame N is
            // acknowledged, so without this nothing ever draws twice.
            //
            // Collected first because sending a frame callback borrows the
            // state that owns the output list. Every output, not the one this
            // timer was created with: a window on a monitor plugged in later
            // has to be told to paint too.
            let outputs: Vec<_> = state.space.outputs().cloned().collect();
            let now = state.start_time.elapsed();
            for output in &outputs {
                for window in state.space.elements() {
                    window.send_frame(output, now, Some(Duration::ZERO), |_, _| {
                        Some(output.clone())
                    });
                }
            }
            state.space.refresh();
            state.popups.cleanup();
            let _ = state.display_handle.flush_clients();
            TimeoutAction::ToDuration(frame_interval)
        })
        .map_err(|e| anyhow::anyhow!("insert headless frame timer: {e}"))?;

    tracing::info!("headless output HEADLESS-1 {width}x{height}");
    Ok(())
}

/// Plug in another virtual output, to the right of the ones already there.
///
/// Returns its name, or `None` if this instance has no headless backend.
///
/// To the right, and appended rather than inserted, because two things have to
/// agree about the order: `output.layout`, which the shell draws its display
/// panel from, and the x coordinates beside it. wlroots got this from
/// `wlr_output_layout_add_auto`; here it is `Space::map_output` at an explicit
/// x, and `Space::outputs()` hands them back in the order they were mapped.
/// A list built backwards is what drew the monitors mirrored while the pointer
/// moved between them the other way — see tests/output-order.test.sh.
pub fn add(state: &mut ViewportState) -> Option<String> {
    let (mode, number) = {
        let headless = state.headless.as_mut()?;
        let number = headless.next;
        headless.next += 1;
        (headless.mode, number)
    };

    let name = format!("HEADLESS-{number}");
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Viewport".into(),
            model: "Headless".into(),
            serial_number: "Unknown".into(),
        },
    );
    let global = output.create_global::<ViewportState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    // Past the right edge of everything already mapped, which for equal-sized
    // outputs is the same arrangement wlr_output_layout_add_auto made.
    let x = state
        .space
        .outputs()
        .filter_map(|other| state.space.output_geometry(other))
        .map(|geometry| geometry.loc.x + geometry.size.w)
        .max()
        .unwrap_or(0);
    state.space.map_output(&output, (x, 0));

    if let Some(headless) = state.headless.as_mut() {
        headless.globals.insert(name.clone(), global);
    }

    tracing::info!("headless output {name} plugged in at x={x}");
    Some(name)
}

/// Unplug a virtual output, or the most recently added one if no name is given.
///
/// Returns whether anything was unplugged. The outputs left keep the positions
/// they had: a monitor going away does not shuffle the others, which is what a
/// real unplug does and what the shell's saved per-output state assumes.
pub fn remove(state: &mut ViewportState, name: Option<&str>) -> bool {
    let Some(headless) = state.headless.as_ref() else {
        return false;
    };

    let target = match name {
        Some(name) => state
            .space
            .outputs()
            .find(|output| output.name() == name)
            .cloned(),
        // The last one mapped, which is the one `add` put furthest right.
        None => state.space.outputs().last().cloned(),
    };
    let Some(output) = target else {
        return false;
    };

    // Only ours. A DRM output cannot be here — the two backends never both
    // exist — but a caller passing the name of something else should be told
    // no rather than have it unmapped.
    if !headless.globals.contains_key(&output.name()) {
        return false;
    }

    state.space.unmap_output(&output);
    if let Some(global) = state
        .headless
        .as_mut()
        .and_then(|headless| headless.globals.remove(&output.name()))
    {
        state.display_handle.remove_global::<ViewportState>(global);
    }

    tracing::info!("headless output {} unplugged", output.name());
    true
}
