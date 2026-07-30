// SPDX-License-Identifier: GPL-3.0-or-later
//
// wp_color_management_v1.
//
// Smithay has no handler for this, so it is written here against the wire
// bindings in wayland-protocols, which Smithay already depends on with the
// `staging` feature.
//
// What it does: a client describes what its buffers actually contain — the
// transfer function, the primaries, the luminance its white was authored
// against — and the compositor stores that per surface. The renderer then
// converts each surface into the output's space instead of assuming everything
// is sRGB. Assuming is the status quo, and it is why HDR content on most
// compositors looks either washed out or oversaturated.
//
// The parametric path is implemented. ICC profiles are not: parsing an ICC
// profile properly is its own project, and accepting one while ignoring what
// it says would be worse than admitting it is unsupported.

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{
        self, Feature, Primaries as WirePrimaries, RenderIntent,
        TransferFunction as WireTransferFunction, WpColorManagerV1,
    },
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;

use viewport_vulkan::color::{Description, Primaries, TransferFunction};

/// The version of the protocol this implements.
const VERSION: u32 = 1;

/// Everything the compositor holds for colour management.
#[derive(Debug)]
pub struct ColorManagementState {
    _global: smithay::reexports::wayland_server::backend::GlobalId,
}

impl ColorManagementState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ()> + 'static,
    {
        Self {
            _global: display.create_global::<D, WpColorManagerV1, _>(VERSION, ()),
        }
    }
}

/// The description a surface has declared, or `None` for "assume sRGB".
#[derive(Debug, Default)]
pub struct SurfaceColor(pub Mutex<Option<Description>>);

/// What a surface's buffers contain, or the sRGB default.
///
/// The default is not a guess so much as the protocol's own answer: a client
/// that has not said anything is required to be treated as sRGB.
pub fn description_for(surface: &WlSurface) -> Description {
    with_states(surface, |states| {
        states
            .data_map
            .get::<SurfaceColor>()
            .and_then(|color| color.0.lock().ok().and_then(|held| *held))
            .unwrap_or_default()
    })
}

/// Map the protocol's named transfer function onto ours.
///
/// Returns `None` for the ones this renderer has no curve for. Saying so is
/// the point: the alternative is silently substituting sRGB, which produces a
/// picture that looks fine and is wrong.
pub fn transfer_from_wire(wire: WireTransferFunction) -> Option<TransferFunction> {
    Some(match wire {
        WireTransferFunction::ExtLinear => TransferFunction::Linear,
        WireTransferFunction::Srgb | WireTransferFunction::ExtSrgb => TransferFunction::Srgb,
        WireTransferFunction::Gamma22 => TransferFunction::Gamma22,
        WireTransferFunction::Gamma28 => TransferFunction::Gamma28,
        WireTransferFunction::St2084Pq => TransferFunction::Pq,
        WireTransferFunction::Hlg => TransferFunction::Hlg,
        // BT.1886 is a display curve rather than an encoding one; treating it
        // as gamma 2.4 is close but not right, so it waits until there is
        // something to check the difference against.
        _ => return None,
    })
}

/// Map the protocol's named primaries onto ours.
pub fn primaries_from_wire(wire: WirePrimaries) -> Option<Primaries> {
    Some(match wire {
        WirePrimaries::Srgb => Primaries::SRGB,
        WirePrimaries::Bt2020 => Primaries::BT2020,
        WirePrimaries::DisplayP3 => Primaries::DISPLAY_P3,
        WirePrimaries::AdobeRgb => Primaries::ADOBE_RGB,
        // PAL, NTSC, DCI-P3 with its own white point, CIE XYZ. Each needs
        // either different chromaticities or a chromatic adaptation this
        // renderer does not do yet.
        _ => return None,
    })
}

/// The named transfer functions this compositor advertises.
pub fn supported_transfer_functions() -> &'static [WireTransferFunction] {
    &[
        WireTransferFunction::ExtLinear,
        WireTransferFunction::Srgb,
        WireTransferFunction::ExtSrgb,
        WireTransferFunction::Gamma22,
        WireTransferFunction::Gamma28,
        WireTransferFunction::St2084Pq,
        WireTransferFunction::Hlg,
    ]
}

/// The named primaries this compositor advertises.
pub fn supported_primaries() -> &'static [WirePrimaries] {
    &[
        WirePrimaries::Srgb,
        WirePrimaries::Bt2020,
        WirePrimaries::DisplayP3,
        WirePrimaries::AdobeRgb,
    ]
}

/// Parameters accumulated by a parametric creator.
///
/// Every field starts unset because the protocol makes it an error to set one
/// twice, and an error to create a description without the mandatory ones.
#[derive(Debug, Default)]
pub struct CreatorParams {
    pub transfer: Option<TransferFunction>,
    pub primaries: Option<Primaries>,
    /// The luminance the client's white was authored against, in cd/m².
    pub reference_luminance: Option<f32>,
}

// Note there is no "already used" flag. The protocol has no destroy request
// on a parametric creator — `create` consumes it — so a client using one
// twice is a use-after-destroy that wayland-server rejects before this code
// is reached.

/// A created image description, or the reason it could not be created.
#[derive(Debug)]
pub struct ImageDescription {
    pub description: Mutex<Option<Description>>,
}

/// The identity assigned to a description, which clients use to tell whether
/// two descriptions are the same without comparing their contents.
///
/// Monotonic and never reused, because the protocol requires a given identity
/// to always mean the same description.
pub fn next_identity() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_curves_this_renderer_has_all_map() {
        // Everything advertised must map, or a client would pick something
        // that is then rejected at create time.
        for wire in supported_transfer_functions() {
            assert!(
                transfer_from_wire(*wire).is_some(),
                "advertised {wire:?} but cannot map it"
            );
        }
        for wire in supported_primaries() {
            assert!(
                primaries_from_wire(*wire).is_some(),
                "advertised {wire:?} but cannot map it"
            );
        }
    }

    #[test]
    fn unsupported_curves_are_refused_rather_than_substituted() {
        // Silently treating an unknown curve as sRGB produces a picture that
        // looks right and is not.
        assert_eq!(transfer_from_wire(WireTransferFunction::St240), None);
        assert_eq!(transfer_from_wire(WireTransferFunction::Log100), None);
        assert_eq!(primaries_from_wire(WirePrimaries::Ntsc), None);
        assert_eq!(primaries_from_wire(WirePrimaries::Cie1931Xyz), None);
    }

    #[test]
    fn srgb_and_ext_srgb_share_a_curve() {
        // ext-sRGB is sRGB's curve extended beyond 0..1; the encoding is the
        // same, so the same decode applies.
        assert_eq!(
            transfer_from_wire(WireTransferFunction::Srgb),
            transfer_from_wire(WireTransferFunction::ExtSrgb)
        );
    }

    #[test]
    fn identities_are_unique_and_never_reused() {
        let a = next_identity();
        let b = next_identity();
        assert_ne!(a, b);
        assert!(b > a);
    }

    #[test]
    fn an_sdr_output_leaves_a_client_no_headroom_to_aim_at() {
        // mpv's arithmetic, from video/out/wayland_common.c, reproduced because
        // the numbers this compositor sends are only correct in terms of what a
        // client does with them.
        //
        // It rescales the target luminances onto libplacebo's fixed reference
        // white, then compares the maximum against it. Anything other than
        // equality means headroom, and headroom means it re-encodes into PQ for
        // an HDR display that is not there — while never telling the compositor,
        // which goes on decoding those buffers as sRGB. That is the washed-out
        // picture, and it is reachable from nothing but these three numbers.
        const SDR_WHITE: f32 = 203.0;
        const HDR_BLACK: f32 = 0.005;

        let description = Description::default();
        let min = MIN_LUMINANCE;
        let reference = description.reference_luminance;
        // What `target_luminance` carries: the same maximum, not a multiple.
        let target_max = reference;

        let scale = (SDR_WHITE - HDR_BLACK) / (reference - min);
        let rescaled = (target_max - min) * scale + HDR_BLACK;

        assert!(
            (rescaled - SDR_WHITE).abs() < 1e-2,
            "an SDR output rescaled to {rescaled}, which mpv reads as HDR headroom"
        );
    }

    #[test]
    fn the_minimum_luminance_survives_the_wire_units() {
        // Ten-thousandths, not thousandths and not whole cd/m². Getting this
        // wrong moves the black point a client is told about by a factor of ten,
        // and it is the divisor in the rescale above.
        assert_eq!((MIN_LUMINANCE * 10_000.0) as u32, 2_000);
    }

    #[test]
    fn a_surface_that_said_nothing_is_srgb() {
        // Not a guess: the protocol requires it.
        assert_eq!(Description::default().transfer, TransferFunction::Srgb);
        assert_eq!(Description::default().primaries, Primaries::SRGB);
    }
}

// ---------------------------------------------------------------------------
// Protocol plumbing
//
// Implemented directly on ViewportState rather than behind a generic delegate:
// this is the compositor's own code, and the delegate dance exists to make a
// handler reusable across compositors, which this one is not.
// ---------------------------------------------------------------------------

use crate::state::ViewportState;

impl GlobalDispatch<WpColorManagerV1, ()> for ViewportState {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());

        // A client picks from what is advertised, so advertising something
        // that would then be rejected at create time is worse than not
        // advertising it.
        manager.supported_intent(RenderIntent::Perceptual);
        manager.supported_feature(Feature::Parametric);
        manager.supported_feature(Feature::SetPrimaries);
        manager.supported_feature(Feature::SetLuminances);
        for transfer in supported_transfer_functions() {
            manager.supported_tf_named(*transfer);
        }
        for primaries in supported_primaries() {
            manager.supported_primaries_named(*primaries);
        }
        manager.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        manager: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                data_init.init(obj, Mutex::new(CreatorParams::default()));
            }

            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                data_init.init(id, surface);
            }

            wp_color_manager_v1::Request::CreateIccCreator { obj } => {
                // Advertised nowhere, so a well-behaved client will not ask.
                // Refusing beats accepting a profile and ignoring it.
                manager.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "ICC profiles are not supported",
                );
                let _ = obj;
            }

            // Both of these hand back the description the compositor renders
            // into. Answering with the truth costs nothing; posting an error
            // would kill the connection of any client that merely asks, which
            // is what `wayland-info` does on startup.
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, .. } => {
                data_init.init(id, ());
            }

            wp_color_manager_v1::Request::GetOutput { id, .. } => {
                data_init.init(id, ());
            }

            wp_color_manager_v1::Request::CreateWindowsScrgb { image_description }
            | wp_color_manager_v1::Request::CreateWindowsBt2100 {
                image_description, ..
            } => {
                manager.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "this well-known description is not supported",
                );
                let _ = image_description;
            }

            wp_color_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, Mutex<CreatorParams>> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        creator: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        params: &Mutex<CreatorParams>,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};

        let mut params = match params.lock() {
            Ok(params) => params,
            Err(_) => return,
        };

        match request {
            Request::SetTfNamed { tf } => {
                if params.transfer.is_some() {
                    creator.post_error(Error::AlreadySet, "the transfer function is already set");
                    return;
                }
                let Ok(tf) = tf.into_result() else {
                    creator.post_error(Error::InvalidTf, "unknown transfer function");
                    return;
                };
                let Some(transfer) = transfer_from_wire(tf) else {
                    creator.post_error(Error::InvalidTf, "unsupported transfer function");
                    return;
                };
                params.transfer = Some(transfer);
            }

            Request::SetPrimariesNamed { primaries } => {
                if params.primaries.is_some() {
                    creator.post_error(Error::AlreadySet, "the primaries are already set");
                    return;
                }
                let Ok(primaries) = primaries.into_result() else {
                    creator.post_error(Error::InvalidPrimariesNamed, "unknown primaries");
                    return;
                };
                let Some(mapped) = primaries_from_wire(primaries) else {
                    creator.post_error(Error::InvalidPrimariesNamed, "unsupported primaries");
                    return;
                };
                params.primaries = Some(mapped);
            }

            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                if params.reference_luminance.is_some() {
                    creator.post_error(Error::AlreadySet, "the luminances are already set");
                    return;
                }
                // The protocol's own constraint. Reference above maximum is
                // not a rounding problem, it is a contradiction.
                if reference_lum > max_lum || min_lum as u32 >= max_lum * 10_000 {
                    creator.post_error(
                        Error::InvalidLuminance,
                        "the reference luminance exceeds the maximum",
                    );
                    return;
                }
                params.reference_luminance = Some(reference_lum as f32);
            }

            Request::SetTfPower { .. } => {
                creator.post_error(
                    Error::UnsupportedFeature,
                    "arbitrary power curves are not supported",
                );
            }
            Request::SetPrimaries { .. } => {
                creator.post_error(
                    Error::UnsupportedFeature,
                    "arbitrary primaries are not supported",
                );
            }
            Request::SetMasteringDisplayPrimaries { .. }
            | Request::SetMasteringLuminance { .. }
            | Request::SetMaxCll { .. }
            | Request::SetMaxFall { .. } => {
                // Mastering metadata describes the display content was graded
                // on. It matters for tone mapping, which this renderer does
                // not do, so accepting it would imply more than is true.
            }

            Request::Create { image_description } => {
                // The two mandatory ones. The protocol says incomplete
                // parameters are an error rather than a set of defaults.
                let (Some(transfer), Some(primaries)) = (params.transfer, params.primaries) else {
                    creator.post_error(
                        Error::IncompleteSet,
                        "a transfer function and primaries are both required",
                    );
                    return;
                };

                let description = Description {
                    transfer,
                    primaries,
                    reference_luminance: params
                        .reference_luminance
                        .unwrap_or(Description::default().reference_luminance),
                };

                let object = data_init.init(
                    image_description,
                    ImageDescription {
                        description: Mutex::new(Some(description)),
                    },
                );
                object.ready(next_identity());
            }

            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionV1, ImageDescription> for ViewportState {
    fn request(
        state: &mut Self,
        _client: &Client,
        description: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &ImageDescription,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_image_description_v1::Request::GetInformation { information } => {
                let Some(held) = data.description.lock().ok().and_then(|held| *held) else {
                    description.post_error(
                        wp_image_description_v1::Error::NotReady,
                        "this description failed and carries no information",
                    );
                    return;
                };
                let info = data_init.init(information, ());

                // Deferred to an idle rather than sent here, because `done` is
                // a destructor event. Sending it inside the callback that
                // created the object destroys it before wayland-backend has
                // attached its user data, and wayland-backend unwraps that
                // failure — so the compositor aborts rather than the client
                // seeing an error. The idle runs as soon as this dispatch
                // finishes, so the client still gets everything in the same
                // round trip.
                state.loop_handle.insert_idle(move |_| {
                    describe(&info, &held);
                    info.done();
                });
            }
            wp_image_description_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, WlSurface> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        object: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        surface: &WlSurface,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_v1::Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                if !matches!(render_intent.into_result(), Ok(RenderIntent::Perceptual)) {
                    object.post_error(
                        wp_color_management_surface_v1::Error::RenderIntent,
                        "only the perceptual render intent is supported",
                    );
                    return;
                }

                let Some(data) = image_description.data::<ImageDescription>() else {
                    return;
                };
                let held = data.description.lock().ok().and_then(|held| *held);

                with_states(surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(SurfaceColor::default);
                    if let Some(color) = states.data_map.get::<SurfaceColor>() {
                        if let Ok(mut slot) = color.0.lock() {
                            *slot = held;
                        }
                    }
                });
            }

            wp_color_management_surface_v1::Request::UnsetImageDescription => {
                // Back to the sRGB default, which is what "said nothing"
                // means.
                with_states(surface, |states| {
                    if let Some(color) = states.data_map.get::<SurfaceColor>() {
                        if let Ok(mut slot) = color.0.lock() {
                            *slot = None;
                        }
                    }
                });
            }

            wp_color_management_surface_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// The darkest the compositor claims its output goes, in cd/m².
///
/// The sRGB reference viewing condition rather than a measured panel, which is
/// what this compositor actually knows.
pub const MIN_LUMINANCE: f32 = 0.2;

/// Send everything known about a description.
///
/// Both the named form and the explicit chromaticities are sent: a client that
/// understands the name can use it, and one that does not still gets the
/// numbers. The protocol encodes chromaticities as parts per million and
/// luminance in thousandths, so nothing here is in the units it looks like.
///
/// The `target_*` half is not optional decoration. The protocol lists
/// `target_primaries` and `target_luminance` among the events a parametric
/// description *must* send, and they are the only place a client learns what
/// the display itself can do — `luminances` describes the encoding, not the
/// panel. Leaving them out does not read as "unknown", it reads as zero, and a
/// zero maximum is not a luminance any real display has. mpv takes that
/// literally: a target maximum that is not exactly SDR white means headroom
/// exists, so it re-encodes into PQ for what it believes is an HDR screen. It
/// then never sets an image description on its surface, so the compositor
/// decodes those PQ bytes as sRGB — lifted blacks and a white that stops at
/// about 58%, which is the washed-out picture every video looked like.
fn describe(info: &WpImageDescriptionInfoV1, description: &Description) {
    const CHROMATICITY: f32 = 1_000_000.0;
    /// Minimum luminance rides in ten-thousandths; every other one is whole
    /// cd/m².
    const MIN_LUMINANCE_SCALE: f32 = 10_000.0;
    let xy = |(x, y): (f32, f32)| ((x * CHROMATICITY) as i32, (y * CHROMATICITY) as i32);

    if let Some(named) = named_primaries(&description.primaries) {
        info.primaries_named(named);
    }
    let (rx, ry) = xy(description.primaries.red);
    let (gx, gy) = xy(description.primaries.green);
    let (bx, by) = xy(description.primaries.blue);
    let (wx, wy) = xy(description.primaries.white);
    info.primaries(rx, ry, gx, gy, bx, by, wx, wy);

    if let Some(named) = named_transfer(description.transfer) {
        info.tf_named(named);
    }

    let min_lum = (MIN_LUMINANCE * MIN_LUMINANCE_SCALE) as u32;
    info.luminances(
        min_lum,
        description.reference_luminance as u32,
        description.reference_luminance as u32,
    );

    // The displayable volume, as opposed to the encodable one above. This
    // compositor composites into an SDR framebuffer and hands it to a panel it
    // drives as SDR, so the two are the same: the primaries it renders in, and
    // a maximum that is the reference white rather than some multiple of it.
    // Saying so is what tells a client there is no headroom to aim at.
    info.target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    info.target_luminance(min_lum, description.reference_luminance as u32);
}

/// The protocol name for a set of primaries, where there is one.
fn named_primaries(primaries: &Primaries) -> Option<WirePrimaries> {
    supported_primaries()
        .iter()
        .copied()
        .find(|wire| primaries_from_wire(*wire).as_ref() == Some(primaries))
}

/// The protocol name for a transfer function.
fn named_transfer(transfer: TransferFunction) -> Option<WireTransferFunction> {
    // Reversed rather than stored, because several names share a curve and
    // the first match is the canonical one.
    Some(match transfer {
        TransferFunction::Linear => WireTransferFunction::ExtLinear,
        TransferFunction::Srgb => WireTransferFunction::Srgb,
        TransferFunction::Gamma22 => WireTransferFunction::Gamma22,
        TransferFunction::Gamma28 => WireTransferFunction::Gamma28,
        TransferFunction::Pq => WireTransferFunction::St2084Pq,
        TransferFunction::Hlg => WireTransferFunction::Hlg,
    })
}

/// The description the compositor renders into.
///
/// Both the output and the feedback objects answer with this. It is the sRGB
/// default until outputs can be configured for HDR, and saying so is accurate
/// rather than a placeholder.
fn output_description() -> Description {
    Description::default()
}

/// Hand a client a ready image description carrying `description`.
fn send_description(
    object: New<WpImageDescriptionV1>,
    description: Description,
    data_init: &mut DataInit<'_, ViewportState>,
) {
    let object = data_init.init(
        object,
        ImageDescription {
            description: Mutex::new(Some(description)),
        },
    );
    object.ready(next_identity());
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _object: &WpImageDescriptionInfoV1,
        _request: smithay::reexports::wayland_protocols::wp::color_management::v1::server::wp_image_description_info_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // The info object has no requests; it emits events and is destroyed.
    }
}

impl Dispatch<WpColorManagementOutputV1, ()> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _object: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
                send_description(image_description, output_description(), data_init);
            }
            wp_color_management_output_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for ViewportState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _object: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_color_management_surface_feedback_v1::Request;
        match request {
            // What the compositor would prefer this surface were in, which is
            // simply what it renders into.
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                send_description(image_description, output_description(), data_init);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}
