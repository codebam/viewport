// SPDX-License-Identifier: GPL-3.0-or-later
//
// The headless backend. Ports the --headless half of src/output.c.
//
// No renderer and no window: a virtual output with a fixed mode, and a timer
// standing in for vblank. Clients connect, map, and get frame callbacks, so
// every part of the window lifecycle and the whole IPC protocol can be driven
// without a GPU or a display — which is what makes the compositor testable in
// CI, and what `output.test_add` exists for.

use std::time::Duration;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::Transform;

use crate::state::ViewportState;

/// The virtual refresh rate, in mHz. Frame callbacks fire at this rate.
const REFRESH: i32 = 60_000;

pub fn init(
    event_loop: &mut EventLoop<'static, ViewportState>,
    state: &mut ViewportState,
    width: i32,
    height: i32,
) -> anyhow::Result<()> {
    let mode = Mode {
        size: (width, height).into(),
        refresh: REFRESH,
    };

    let output = Output::new(
        "HEADLESS-1".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Viewport".into(),
            model: "Headless".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<ViewportState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    state.active_output = Some(output.name());

    let frame_interval = Duration::from_micros(1_000_000_000 / REFRESH as u64);

    event_loop
        .handle()
        .insert_source(Timer::from_duration(frame_interval), move |_, _, state| {
            // A client will not paint frame N+1 until frame N is
            // acknowledged, so without this nothing ever draws twice.
            for window in state.space.elements() {
                window.send_frame(
                    &output,
                    state.start_time.elapsed(),
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
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
