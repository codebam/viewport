// SPDX-License-Identifier: GPL-3.0-or-later
//
// The headless backend. Ports the --headless half of src/output.c.
//
// No window: virtual outputs with a fixed mode, and a timer standing in for
// vblank. Clients connect, map, and get frame callbacks, so every part of the
// window lifecycle and the whole IPC protocol can be driven without a display
// — which is what makes the compositor testable in CI, and what
// `output.test_add` exists for.
//
// There is a renderer, though nothing scans out of it. Every capture path in
// this compositor composites where the renderer is, so a backend without one
// can accept a screencopy request and never answer it: the frames queue and
// nothing drains them. wlroots did not have this problem — its headless
// backend takes WLR_RENDERER like any other — which is why the C build passes
// tests/capture.test.sh on a machine with no display and this one did not.
//
// GLES and not Vulkan, deliberately. See `renderer()`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
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
    pub(crate) globals: HashMap<String, GlobalId>,
    pub(crate) outputs: BTreeMap<String, Output>,
    pub(crate) disabled: HashSet<String>,
    /// The number the next output gets. Only ever counts up: a name that comes
    /// back after an unplug is a different monitor to anything holding the old
    /// one, and reusing it is how a stale reference starts looking valid.
    next: u32,
    /// What captures are composited with, when there is one.
    ///
    /// `None` on a machine with no EGL at all — which is a compositor that
    /// still runs every IPC and window-lifecycle test and answers no to
    /// anything wanting pixels, rather than one that refuses to start.
    ///
    /// Moved out and back around a render, because compositing borrows the
    /// state this lives in. The DRM backend does the same thing with
    /// `Gpu::Placeholder`.
    renderer: Option<GlesRenderer>,
}

/// Open a renderer that needs neither a display nor a device node.
///
/// GLES, and not the Vulkan renderer this compositor draws with everywhere
/// else. Vulkan looks like the shorter path — Smithay's `VulkanAllocator`
/// wants no GBM device and no DRM node — but it allocates through
/// `VK_EXT_image_drm_format_modifier`, which lavapipe does not implement (the
/// same reason `Gpu::Gles` exists at all) and a hosted CI runner has no
/// `/dev/dri` for anything else to bind. It would work on a workstation and
/// fail in CI, which is the half a test exists for.
///
/// GLES offscreen targets are `GlesRenderbuffer`s, which involve no DMA-BUF,
/// so software Mesa serves them with no device node at all. It is also already
/// the pair the nested backend captures with, so nothing downstream changes.
fn renderer() -> anyhow::Result<GlesRenderer> {
    // EGL_MESA_platform_surfaceless: a display with no window system behind
    // it. Mesa has had it for years and it is what every headless GL test
    // harness uses; a driver without it has no other way to give us a context
    // without a device node.
    //
    // Behind `catch_unwind` because Smithay loads libEGL through a `LazyLock`
    // that `.expect()`s — on a machine with no libEGL.so.1 the first EGL call
    // of any kind panics, and there is no error to return instead. Unwinding
    // out of here would take down a compositor that was about to run every
    // test that does not want pixels. The panic message is printed by the
    // default hook before this catches it, which is the diagnosis, so nothing
    // is swallowed.
    //
    // SAFETY: `EGLSurfacelessDisplay` names the default display, which has no
    // lifetime to outlive, and the context below is the only user of it.
    let display = std::panic::catch_unwind(|| unsafe {
        EGLDisplay::new(smithay::backend::egl::native::EGLSurfacelessDisplay)
    })
    .map_err(|_| anyhow::anyhow!("the EGL library could not be loaded at all"))?
    .map_err(|e| {
        anyhow::anyhow!(
            "no surfaceless EGL display ({e}). The headless backend renders through \
             EGL_MESA_platform_surfaceless; mesa provides it, and without a driver that \
             does there is nothing to composite a screenshot with."
        )
    })?;

    let context =
        EGLContext::new(&display).map_err(|e| anyhow::anyhow!("creating an EGL context: {e}"))?;

    // SAFETY: the context was just created, is current on no other thread, and
    // is moved into the renderer, which owns it from here.
    let renderer = unsafe { GlesRenderer::new(context) }
        .map_err(|e| anyhow::anyhow!("creating the OpenGL renderer: {e}"))?;

    Ok(renderer)
}

pub fn init(
    event_loop: &mut EventLoop<'static, ViewportState>,
    state: &mut ViewportState,
    width: i32,
    height: i32,
) -> anyhow::Result<()> {
    // Warned about rather than fatal. Everything except pixels still works
    // without it, and most of what runs headless — the IPC, the layout, the
    // window lifecycle — never asks for any.
    let renderer = match renderer() {
        Ok(renderer) => {
            tracing::info!("headless renderer: OpenGL on a surfaceless EGL display");
            Some(renderer)
        }
        Err(e) => {
            tracing::warn!("no headless renderer ({e:#}); captures will fail");
            None
        }
    };

    state.headless = Some(Headless {
        mode: Mode {
            size: (width, height).into(),
            refresh: REFRESH,
        },
        globals: HashMap::new(),
        outputs: BTreeMap::new(),
        disabled: HashSet::new(),
        next: 1,
        renderer,
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
            // The out-of-process shell is not in the space — it is drawn under
            // it — so it has to be invited by name or it paints once and stops.
            let shell = state.shell_client_surfaces();
            // And the wallpaper, for the same reason: not in the space, drawn
            // under it, and otherwise painting one frame and stopping.
            let background = state.background_surfaces();
            for output in &outputs {
                for window in state.space.elements() {
                    window.send_frame(output, now, Some(Duration::ZERO), |_, _| {
                        Some(output.clone())
                    });
                }
                for surface in &shell {
                    smithay::desktop::utils::send_frames_surface_tree(
                        surface,
                        output,
                        now,
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
                for surface in &background {
                    smithay::desktop::utils::send_frames_surface_tree(
                        surface,
                        output,
                        now,
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
            }
            state.space.refresh();
            state.popups.cleanup();
            service_captures(state, &outputs);
            let _ = state.display_handle.flush_clients();
            TimeoutAction::ToDuration(frame_interval)
        })
        .map_err(|e| anyhow::anyhow!("insert headless frame timer: {e}"))?;

    tracing::info!("headless output HEADLESS-1 {width}x{height}");

    // After the first output exists, because the shell is configured to the
    // size of the layout and a layout with no outputs in it has none. This is
    // also what makes the out-of-process backend testable without a screen:
    // the shell is a client, and a headless compositor is still a compositor.
    state.start_shell_process();
    // And the wallpaper terminal, if one was asked for. Beside the shell
    // because it needs the same thing the shell does: outputs, so it can be
    // told how big the desktop is.
    state.start_background_process();
    Ok(())
}

/// Answer everything waiting on pixels, for every output.
///
/// On the frame timer rather than driven by damage. The other two backends
/// service captures at the end of a render they were going to do anyway; this
/// one has no screen to draw to, so there is no such render, and a screenshot
/// of an idle desktop is the ordinary case for a headless compositor. Sixty
/// times a second costs nothing while nothing is waiting — each of these
/// returns immediately on an empty queue.
fn service_captures(state: &mut ViewportState, outputs: &[Output]) {
    // Moved out and back: compositing borrows the state the renderer lives in.
    // The DRM backend does the same thing, where the hole is `Gpu::Placeholder`
    // and using it panics; here it is a `None` that reads as "no renderer",
    // so a reentrant call gets the same answer a machine without EGL does.
    let Some(mut renderer) = state.headless.as_mut().and_then(|h| h.renderer.take()) else {
        return;
    };

    for output in outputs {
        state.service_screencopy::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
            output,
            &mut renderer,
        );
        state.service_image_capture::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
            output,
            &mut renderer,
        );
        state.feed_casts::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
            output,
            &mut renderer,
        );
    }

    if let Some(headless) = state.headless.as_mut() {
        headless.renderer = Some(renderer);
    }
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
        headless.outputs.insert(name.clone(), output.clone());
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
        Some(name) => headless.outputs.get(name).cloned(),
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
    if let Some(headless) = state.headless.as_mut() {
        headless.outputs.remove(&output.name());
        headless.disabled.remove(&output.name());
    }
    state.output_removed(&output.name());

    tracing::info!("headless output {} unplugged", output.name());
    true
}
