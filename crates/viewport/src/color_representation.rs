// SPDX-License-Identifier: GPL-3.0-or-later
//
// wp_color_representation_v1.
//
// Smithay has no handler for this either, so — like `color_management` — it is
// written here against the wire bindings in wayland-protocols.
//
// What it does: a client says what the code words in its Y′CbCr buffers
// *mean* — the matrix they were encoded with, whether they fill 0..=1 or the
// broadcast 16..=235 slice, where downsampled chroma sits relative to luma.
// None of that is carried by a DMA-BUF, which left every compositor guessing
// the same way: matrix from the picture's height, range narrow, siting the
// MPEG default. The guess is right almost always and wrong exactly where
// nobody notices immediately — a washed-out frame is easy to blame on the
// file.
//
// The state lands in the surface's data map as a
// [`viewport_vulkan::color::SurfaceRepresentation`], and the renderer reads
// it when it imports the buffer — the same arrangement `wp-color-management-v1`
// uses for `SurfaceColor`, and for the same reason: the import path is handed
// the surface's `SurfaceData` and nothing else, so a type defined over here
// could not be looked up from there.
//
// What this compositor does *not* advertise is what its Vulkan sampler
// cannot be told: identity coefficients, FCC, SMPTE 240, BT.2020 constant
// luminance, ICtCp, and H.273 chroma types 4 and 5, which place chroma a
// full row away from anything Vulkan can name. A client asking for one is
// refused at the request rather than accepted and quietly ignored.

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_representation::v1::server::{
    wp_color_representation_manager_v1::{self, WpColorRepresentationManagerV1},
    wp_color_representation_surface_v1::{
        self, AlphaMode as WireAlphaMode, ChromaLocation as WireChromaLocation,
        Coefficients as WireCoefficients, Range as WireRange, WpColorRepresentationSurfaceV1,
    },
};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::{with_states, BufferAssignment, SurfaceAttributes};

pub use viewport_vulkan::color::{
    ChromaLocation, ChromaSiting, Coefficients, Range, Representation, SurfaceRepresentation,
};

/// The version of the protocol this implements.
const VERSION: u32 = 1;

/// Everything the compositor holds for colour representation.
#[derive(Debug)]
pub struct ColorRepresentationState {
    _global: GlobalId,
    /// The live per-surface objects, for the two things the protocol asks of
    /// the compositor and no one else can answer: at most one object per
    /// surface (`surface_exists`), and a commit-time format check that has to
    /// raise its error on the right object.
    surfaces: Vec<(WpColorRepresentationSurfaceV1, WlSurface)>,
}

impl ColorRepresentationState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorRepresentationManagerV1, ()> + 'static,
    {
        Self {
            _global: display.create_global::<D, WpColorRepresentationManagerV1, _>(VERSION, ()),
            surfaces: Vec::new(),
        }
    }

    /// Drop the objects whose client or surface has gone.
    fn reap(&mut self) {
        self.surfaces
            .retain(|(object, surface)| object.is_alive() && surface.is_alive());
    }

    /// The live object for a surface, if one exists.
    fn object_for(&self, surface: &WlSurface) -> Option<WpColorRepresentationSurfaceV1> {
        self.surfaces
            .iter()
            .find(|(_, held)| held == surface)
            .map(|(object, _)| object.clone())
    }
}

/// The alpha modes this compositor advertises.
///
/// Only the one every Wayland surface already gets composited as. Straight
/// alpha changes what the blending equation expects of the buffer's RGB
/// channels, and this compositor's paths — the web shell's DMA-BUF included
/// — all speak premultiplied.
pub fn supported_alpha_modes() -> &'static [WireAlphaMode] {
    &[WireAlphaMode::PremultipliedElectrical]
}

/// The coefficient-and-range pairs this compositor advertises.
///
/// Exactly the matrices its Vulkan `VkSamplerYcbcrConversion` can be told,
/// each across both ranges. Identity is deliberately absent: it names no
/// conversion — an RGB buffer is already what the renderer draws — so
/// advertising it would promise a check this renderer never performs, and
/// refusing it costs no real client, because a client with RGB buffers has
/// nothing to declare.
pub fn supported_coefficients_and_ranges() -> &'static [(WireCoefficients, WireRange)] {
    &[
        (WireCoefficients::Bt709, WireRange::Full),
        (WireCoefficients::Bt709, WireRange::Limited),
        (WireCoefficients::Bt601, WireRange::Full),
        (WireCoefficients::Bt601, WireRange::Limited),
        (WireCoefficients::Bt2020, WireRange::Full),
        (WireCoefficients::Bt2020, WireRange::Limited),
    ]
}

/// Map the protocol's named coefficients onto the renderer's.
///
/// `None` for everything not advertised, and the request handler raises the
/// protocol's own error rather than substituting a nearby matrix: BT.601 and
/// BT.709 differ by a green cast on exactly the material a declaration is
/// most likely to arrive for.
pub fn coefficients_from_wire(wire: WireCoefficients) -> Option<Coefficients> {
    Some(match wire {
        WireCoefficients::Bt709 => Coefficients::Bt709,
        WireCoefficients::Bt601 => Coefficients::Bt601,
        WireCoefficients::Bt2020 => Coefficients::Bt2020,
        // Identity (no conversion), FCC, SMPTE 240, BT.2020 CL, ICtCp: none
        // of these is a thing a Vulkan sampler conversion can be asked to do.
        _ => return None,
    })
}

/// Map the protocol's range onto the renderer's.
pub fn range_from_wire(wire: WireRange) -> Option<Range> {
    Some(match wire {
        WireRange::Full => Range::Full,
        WireRange::Limited => Range::Limited,
        // 0 is invalid and never on the wire for a well-formed request; the
        // generated `into_result` catches it before this is reached.
        _ => return None,
    })
}

/// Map one H.273 chroma sample location onto its two Vulkan axes.
///
/// `None` for types 4 and 5, which site chroma a full row below (or between)
/// the luma rows: `VkChromaLocation` names only cosited-even and midpoint,
/// so there is nothing to ask the sampler for.
pub fn siting_from_wire(wire: WireChromaLocation) -> Option<ChromaSiting> {
    let (horizontal, vertical) = match wire {
        WireChromaLocation::Type0 => (ChromaLocation::CositedEven, ChromaLocation::Midpoint),
        WireChromaLocation::Type1 => (ChromaLocation::Midpoint, ChromaLocation::Midpoint),
        WireChromaLocation::Type2 => (ChromaLocation::CositedEven, ChromaLocation::CositedEven),
        WireChromaLocation::Type3 => (ChromaLocation::Midpoint, ChromaLocation::CositedEven),
        _ => return None,
    };
    Some(ChromaSiting {
        horizontal,
        vertical,
    })
}

/// The colour representation a client has set but whose commit has not
/// landed yet.
///
/// Double-buffered like every other surface state in the protocol, and
/// parked here for the same reason `PendingSurfaceColor` is: a declaration
/// applies with the commit that carries it, not a frame early. It also
/// carries the compatibility check against the buffer that commit attached.
#[derive(Debug, Default)]
pub struct PendingRepresentation(pub Mutex<Option<Representation>>);

/// Swap the parked representation into the live one, and hold the commit to
/// it.
///
/// Runs from the post-commit hook registered by `get_surface`. The value
/// stays parked rather than being taken, so a later commit that carries no
/// new request keeps the last declaration — which is what double-buffered
/// state does when it is committed again, and what makes the declaration
/// survive a client that only ever says it once.
fn apply_pending_representation(
    surface: &WlSurface,
    object: Option<&WpColorRepresentationSurfaceV1>,
) {
    with_states(surface, |states| {
        let pending = states
            .data_map
            .get::<PendingRepresentation>()
            .and_then(|pending| pending.0.lock().ok().and_then(|slot| *slot));
        states
            .data_map
            .insert_if_missing_threadsafe(SurfaceRepresentation::default);
        if let Some(held) = states.data_map.get::<SurfaceRepresentation>() {
            if let Ok(mut slot) = held.0.lock() {
                *slot = pending;
            }
        }

        // The protocol's own verification: the pixel format and the values
        // set have to be about the same family of pixels. A Y′CbCr
        // declaration against an RGB buffer has no meaning, and the client
        // that committed one has said something contradictory — which is its
        // error to raise, its connection to lose, and the only way a wrong
        // declaration stops being a quiet one.
        let Some(_declaration) = pending.filter(|declaration| !declaration.is_empty()) else {
            return;
        };
        let Some(object) = object else {
            return;
        };
        // An shm buffer is RGB by definition; a DMA-BUF is YUV exactly when
        // its fourcc says so. Either way the answer comes from the format,
        // not from a guess about the client. The buffer read here is the one
        // this very commit attached: `current()` in a post-commit hook is the
        // state that just landed.
        let mut state = states.cached_state.get::<SurfaceAttributes>();
        let attributes = state.current();
        let buffer = match attributes.buffer.as_ref() {
            Some(BufferAssignment::NewBuffer(buffer)) => buffer,
            // No buffer, or `Removed`: there is nothing to contradict.
            _ => return,
        };
        let ycbcr_buffer = smithay::wayland::dmabuf::get_dmabuf(buffer)
            .ok()
            .is_some_and(|dmabuf| {
                use smithay::backend::allocator::Buffer as _;
                viewport_vulkan::format::is_yuv(dmabuf.format().code)
            });
        if !ycbcr_buffer {
            object.post_error(
                wp_color_representation_surface_v1::Error::PixelFormat,
                "a Y'CbCr declaration was made for a buffer that is not Y'CbCr",
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Protocol plumbing
//
// Implemented directly on ViewportState rather than behind a generic
// delegate, exactly as `color_management` is: this is the compositor's own
// code, and the delegate dance exists to make a handler reusable across
// compositors, which this one is not.
// ---------------------------------------------------------------------------

use crate::state::ViewportState;

impl GlobalDispatch<WpColorRepresentationManagerV1, ()> for ViewportState {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorRepresentationManagerV1>,
        _global: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());

        // The protocol wants the advertisement sent whole at bind time, then
        // `done`. A client picks from it, so as everywhere else the list and
        // the mappings above have to stay in agreement — which is what the
        // test below enforces.
        for mode in supported_alpha_modes() {
            manager.supported_alpha_mode(*mode);
        }
        for (coefficients, range) in supported_coefficients_and_ranges() {
            manager.supported_coefficients_and_ranges(*coefficients, *range);
        }
        manager.done();
    }
}

impl Dispatch<WpColorRepresentationManagerV1, ()> for ViewportState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &WpColorRepresentationManagerV1,
        request: wp_color_representation_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_representation_manager_v1::Request::GetSurface { id, surface } => {
                state.color_representation.reap();
                if state.color_representation.object_for(&surface).is_some() {
                    // One object per surface, for the surface's life — or
                    // rather its object's, since destroy ends the claim.
                    manager.post_error(
                        wp_color_representation_manager_v1::Error::SurfaceExists,
                        "a colour representation already exists for this surface",
                    );
                    let _ = id;
                    return;
                }
                let object = data_init.init(id, surface.clone());
                state
                    .color_representation
                    .surfaces
                    .push((object.clone(), surface.clone()));

                // The hook is registered once per surface ever: it reads the
                // parked value, not a captured object, apart from where it
                // has to raise an error — and an error belongs to whichever
                // object made the declaration it rejects, so the object is
                // captured and re-looked-up at commit (see
                // `Dispatch<WpColorRepresentationSurfaceV1, _>` for the
                // re-registration that keeps it current).
                let first = with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(PendingRepresentation::default)
                });
                if first {
                    smithay::wayland::compositor::add_post_commit_hook::<ViewportState, _>(
                        &surface,
                        move |state, _, surface| {
                            let held = state.color_representation.object_for(surface);
                            apply_pending_representation(surface, held.as_ref());
                        },
                    );
                }
            }
            wp_color_representation_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpColorRepresentationSurfaceV1, WlSurface> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        object: &WpColorRepresentationSurfaceV1,
        request: wp_color_representation_surface_v1::Request,
        surface: &WlSurface,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_color_representation_surface_v1::{Error, Request};

        // The surface is gone: the object is inert, and every request on an
        // inert object is the protocol error rather than silence.
        if !surface.is_alive() {
            object.post_error(Error::Inert, "the surface of this object is destroyed");
            return;
        }

        match request {
            Request::SetAlphaMode { alpha_mode } => {
                let Ok(mode) = alpha_mode.into_result() else {
                    object.post_error(Error::AlphaMode, "unknown alpha mode");
                    return;
                };
                if !supported_alpha_modes().contains(&mode) {
                    // The request is only legal for advertised modes; say so
                    // rather than accepting a promise this compositor cannot
                    // keep — straight alpha changes the blending equation,
                    // and every path here already premultiplies.
                    object.post_error(Error::AlphaMode, "this alpha mode is not supported");
                }
                // `premultiplied_electrical` is what already happens to
                // every surface; double-buffered or not, the declaration
                // asks for the status quo and parking it changes nothing.
            }

            Request::SetCoefficientsAndRange {
                coefficients,
                range,
            } => {
                let Ok(coefficients) = coefficients.into_result() else {
                    object.post_error(Error::Coefficients, "unknown matrix coefficients");
                    return;
                };
                let Ok(range) = range.into_result() else {
                    object.post_error(Error::Coefficients, "unknown range");
                    return;
                };
                if !supported_coefficients_and_ranges().contains(&(coefficients, range)) {
                    object.post_error(
                        Error::Coefficients,
                        "this coefficients-and-range combination is not supported",
                    );
                    return;
                }
                // The conversions above cannot fail for an advertised
                // combination — the test that they do not is what keeps that
                // true rather than merely likely. A declaration already in
                // flight keeps its chroma siting; these two fields arrive
                // together and replace each other together.
                let mut value = current_declaration(surface).unwrap_or_default();
                value.coefficients = coefficients_from_wire(coefficients);
                value.range = range_from_wire(range);
                park(surface, Some(value));
            }

            Request::SetChromaLocation { chroma_location } => {
                let Ok(location) = chroma_location.into_result() else {
                    object.post_error(Error::ChromaLocation, "unknown chroma location");
                    return;
                };
                let Some(siting) = siting_from_wire(location) else {
                    object.post_error(
                        Error::ChromaLocation,
                        "this chroma location has no Vulkan equivalent",
                    );
                    return;
                };
                // Siting is its own double-buffered field, so setting it
                // before any coefficients parks a declaration whose matrix
                // half is still the renderer's guess — and setting it after
                // updates that declaration without disturbing the matrix.
                let mut value = current_declaration(surface).unwrap_or_default();
                value.chroma = Some(siting);
                park(surface, Some(value));
            }

            Request::Destroy => {
                // "Destroying this object unsets all the colour
                // representation metadata from the surface" — and unsetting
                // is double-buffered, so it is parked rather than applied on
                // the spot. The object goes with the request; the renderer
                // reverts to inference when the next commit lands.
                park(surface, None);
            }
            _ => {}
        }
    }
}

/// The declaration a surface is currently heading towards: what is parked
/// for the next commit if anything is, and otherwise what the commit before
/// it settled on.
///
/// The requests write single fields of this, so it has to be readable before
/// the commit that applies it — a client that sets the siting twice in a
/// row, or the siting and then the matrix, amends one running declaration
/// rather than starting a second.
fn current_declaration(surface: &WlSurface) -> Option<Representation> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<PendingRepresentation>()
            .and_then(|pending| pending.0.lock().ok().and_then(|held| *held))
            .or_else(|| {
                states
                    .data_map
                    .get::<SurfaceRepresentation>()
                    .and_then(|held| held.0.lock().ok().and_then(|value| *value))
            })
    })
}

/// Park a declaration for the next commit.
fn park(surface: &WlSurface, representation: Option<Representation>) {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(PendingRepresentation::default);
        if let Some(pending) = states.data_map.get::<PendingRepresentation>() {
            if let Ok(mut slot) = pending.0.lock() {
                *slot = representation;
            }
        }
    });
}

// Keep the hook pointed at the live object across a destroy-and-recreate:
// `GetSurface` re-looks-up the surface's object at every commit (see the
// closure in `Dispatch<WpColorRepresentationManagerV1, ()>`), so a
// declaration parked after recreation is validated against the object that
// made it, and a dead one simply has no object to post on.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_pair_maps_to_the_renderer() {
        // The request handler treats "advertised" as "mappable" and parks the
        // mapped value without a fallible path. If an advertised combination
        // ever mapped to None, a compliant client's declaration would vanish
        // silently — the exact quiet wrong this protocol exists to end.
        for (coefficients, range) in supported_coefficients_and_ranges() {
            assert!(
                coefficients_from_wire(*coefficients).is_some(),
                "advertised {coefficients:?} but cannot map it"
            );
            assert!(
                range_from_wire(*range).is_some(),
                "advertised {range:?} but cannot map it"
            );
        }
    }

    #[test]
    fn unadvertised_coefficients_are_refused_rather_than_substituted() {
        // BT.601 substituted for ICtCp, or any matrix for "identity", is a
        // wrong picture with a straight face on it.
        assert_eq!(coefficients_from_wire(WireCoefficients::Identity), None);
        assert_eq!(coefficients_from_wire(WireCoefficients::Fcc), None);
        assert_eq!(coefficients_from_wire(WireCoefficients::Smpte240), None);
        assert_eq!(coefficients_from_wire(WireCoefficients::Bt2020Cl), None);
        assert_eq!(coefficients_from_wire(WireCoefficients::Ictcp), None);
    }

    #[test]
    fn the_three_advertised_matrices_are_the_three_the_sampler_takes() {
        assert_eq!(
            coefficients_from_wire(WireCoefficients::Bt709),
            Some(Coefficients::Bt709)
        );
        assert_eq!(
            coefficients_from_wire(WireCoefficients::Bt601),
            Some(Coefficients::Bt601)
        );
        assert_eq!(
            coefficients_from_wire(WireCoefficients::Bt2020),
            Some(Coefficients::Bt2020)
        );
    }

    #[test]
    fn the_four_expressible_chroma_sitings_land_on_the_axes_vulkan_names() {
        // H.273 0 is what the MPEG family and hardware decoders use; getting
        // horizontal and vertical the wrong way round is a chroma shift
        // nobody can unsee once pointed out, so both axes are pinned.
        assert_eq!(
            siting_from_wire(WireChromaLocation::Type0),
            Some(ChromaSiting {
                horizontal: ChromaLocation::CositedEven,
                vertical: ChromaLocation::Midpoint,
            })
        );
        assert_eq!(
            siting_from_wire(WireChromaLocation::Type1),
            Some(ChromaSiting {
                horizontal: ChromaLocation::Midpoint,
                vertical: ChromaLocation::Midpoint,
            })
        );
        assert_eq!(
            siting_from_wire(WireChromaLocation::Type2),
            Some(ChromaSiting {
                horizontal: ChromaLocation::CositedEven,
                vertical: ChromaLocation::CositedEven,
            })
        );
        assert_eq!(
            siting_from_wire(WireChromaLocation::Type3),
            Some(ChromaSiting {
                horizontal: ChromaLocation::Midpoint,
                vertical: ChromaLocation::CositedEven,
            })
        );
    }

    #[test]
    fn chroma_sitings_vulkan_cannot_name_are_refused() {
        assert_eq!(siting_from_wire(WireChromaLocation::Type4), None);
        assert_eq!(siting_from_wire(WireChromaLocation::Type5), None);
    }

    #[test]
    fn only_the_alpha_mode_every_surface_already_gets_is_advertised() {
        // If a second mode were ever advertised, `set_alpha_mode` would have
        // to do more than accept it — this pins the assumption it rests on.
        assert_eq!(
            supported_alpha_modes(),
            &[WireAlphaMode::PremultipliedElectrical]
        );
    }
}
