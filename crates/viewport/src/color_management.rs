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

use smithay::output::Output;
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
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;

pub use viewport_vulkan::color::SurfaceColor;
use viewport_vulkan::color::{Description, Primaries, TransferFunction};

/// The version of the protocol this implements.
const VERSION: u32 = 1;

/// Everything the compositor holds for colour management.
#[derive(Debug)]
pub struct ColorManagementState {
    _global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The per-output objects clients hold.
    ///
    /// Kept because the protocol gives a client no way to notice on its own:
    /// it fetches the output's image description once, when it connects, and
    /// only asks again if it is told the description changed. Without this
    /// list an output switched into HDR stays SDR to every client already
    /// running — and to every client started afterwards, since nothing else
    /// consulted the output's state either.
    outputs: Vec<(WpColorManagementOutputV1, WlOutput)>,
    /// The per-surface feedback objects, for the same reason.
    feedback: Vec<Feedback>,
}

/// One client's feedback object, and the last answer it was given.
#[derive(Debug)]
struct Feedback {
    object: WpColorManagementSurfaceFeedbackV1,
    surface: WlSurface,
    /// The identity last announced, so an unchanged answer is not re-sent.
    /// A client is entitled to fetch a new description on every event, and
    /// this runs on every layout.
    last: Option<u32>,
}

impl ColorManagementState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ()> + 'static,
    {
        Self {
            _global: display.create_global::<D, WpColorManagerV1, _>(VERSION, ()),
            outputs: Vec::new(),
            feedback: Vec::new(),
        }
    }

    /// Drop the objects whose client has gone.
    fn reap(&mut self) {
        self.outputs.retain(|(object, _)| object.is_alive());
        self.feedback.retain(|entry| entry.object.is_alive());
    }
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
        state: &mut Self,
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
            //
            // Both are kept, with what they were asked about: the description
            // depends on which output is involved, and the client has to be
            // told when that output changes.
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
                let object = data_init.init(id, surface.clone());
                state.color_management.reap();
                state.color_management.feedback.push(Feedback {
                    object,
                    surface,
                    last: None,
                });
            }

            wp_color_manager_v1::Request::GetOutput { id, output } => {
                let object = data_init.init(id, output.clone());
                state.color_management.reap();
                state.color_management.outputs.push((object, output));
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
                //
                // Saturating, because the minimum rides in ten-thousandths and
                // the maximum in whole cd/m²: putting them in the same units
                // overflows a u32 above about 429,000 cd/m², which is a number
                // a client is free to send and which would otherwise panic the
                // compositor rather than fail the request.
                if reference_lum > max_lum || min_lum >= max_lum.saturating_mul(10_000) {
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

/// What PQ's encoded 1.0 means, in cd/m².
///
/// Fixed by ST 2084, so it is the maximum of the *encoding* whatever panel is
/// attached.
const PQ_PEAK: f32 = 10_000.0;

/// The peak this compositor claims an HDR output reaches, in cd/m².
///
/// Not measured. The connector's HDR metadata blob carries zeroes for the
/// mastering display — `hdr::hdr_metadata_bytes` sends what the C version sent
/// — so there is no panel figure to pass on. Some maximum above reference
/// white has to be sent regardless: a client compares the target maximum with
/// reference white, and equality means no headroom, which is the answer that
/// keeps it in SDR. 1000 is the level HDR10 content is graded against and the
/// figure a consumer HDR panel is at least asked to approximate, so it is the
/// least wrong number available without reading the EDID.
pub const HDR_PEAK_LUMINANCE: f32 = 1000.0;

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

    let (min_lum, max_lum, reference_lum) = luminance_args(description);
    info.luminances(min_lum, max_lum, reference_lum);

    // The displayable volume, as opposed to the encodable one above.
    //
    // Driven as SDR the two are the same: the primaries composited in, and a
    // maximum that is reference white rather than some multiple of it. Saying
    // so is what tells a client there is no headroom to aim at.
    //
    // Driven as HDR they are not, and saying they are is what kept every
    // client in SDR on a screen that was already in HDR. A client reads this
    // pair and nothing else to decide whether the display can show more than
    // reference white — Chrome will not offer a page HDR video without it, and
    // mpv will not re-encode into PQ — so an HDR output has to report the peak
    // it actually aims at.
    info.target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    let (target_min, target_max) = target_luminance_args(description);
    info.target_luminance(target_min, target_max);
}

/// The three arguments of `luminances`, in the order the protocol takes them:
/// minimum, maximum, reference white.
///
/// Split out to be testable. The maximum and reference white were the same
/// number while every description this sent was SDR, so their order was
/// unobservable — and swapping them told a client its PQ reference white was
/// 10,000 cd/m², which is a display that does not exist.
fn luminance_args(description: &Description) -> (u32, u32, u32) {
    /// The minimum rides in ten-thousandths; the other two are whole cd/m².
    const MIN_LUMINANCE_SCALE: f32 = 10_000.0;
    (
        (MIN_LUMINANCE * MIN_LUMINANCE_SCALE) as u32,
        encodable_peak(description) as u32,
        description.reference_luminance as u32,
    )
}

/// The two arguments of `target_luminance`: minimum, then maximum.
fn target_luminance_args(description: &Description) -> (u32, u32) {
    let (min_lum, ..) = luminance_args(description);
    (min_lum, displayable_peak(description) as u32)
}

/// The brightest value the encoding itself can carry, in cd/m².
///
/// PQ is absolute and tops out at 10,000 whatever is attached; every other
/// curve here is relative, so its maximum *is* reference white.
fn encodable_peak(description: &Description) -> f32 {
    if description.transfer.is_absolute() {
        PQ_PEAK
    } else {
        description.reference_luminance
    }
}

/// The brightest the display this describes is claimed to reach, in cd/m².
fn displayable_peak(description: &Description) -> f32 {
    if description.transfer.is_absolute() {
        HDR_PEAK_LUMINANCE
    } else {
        description.reference_luminance
    }
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

/// What the compositor renders into for a given output.
///
/// This is the answer both the output objects and the surface feedback objects
/// give, and it is the whole of what a client knows about the screen. It has
/// to follow the output's actual state: an output in HDR is being driven with
/// BT.2020 primaries and a PQ curve, and `udev::render` already sets exactly
/// this on the renderer before drawing that output's frame. Answering sRGB
/// while driving PQ is not a placeholder, it is a different picture from the
/// one the client is told to draw.
pub fn output_description(state: &ViewportState, output: Option<&Output>) -> Description {
    let hdr = output
        .map(|output| state.hdr_enabled(&output.name()))
        .unwrap_or(false);
    if hdr {
        hdr_description()
    } else {
        Description::default()
    }
}

/// What an output in HDR is being driven as.
///
/// The same three values `udev::render` hands the renderer, so a client that
/// matches this description is handed straight through instead of converted.
pub fn hdr_description() -> Description {
    Description {
        primaries: Primaries::BT2020,
        transfer: TransferFunction::Pq,
        reference_luminance: Description::default().reference_luminance,
    }
}

/// How many distinct descriptions the identity table remembers.
///
/// The parametric creator lets a client mint a description around any
/// reference luminance it likes, so the table cannot be allowed to grow with
/// the requests: an unbounded one is a client-controlled allocation that never
/// shrinks. 1024 is far above what a real session produces — SDR, HDR, and a
/// handful of descriptions a video player pins — while making eviction
/// something a malicious client can do to itself and nobody else.
const KNOWN_DESCRIPTIONS: usize = 1024;

/// The identity for a description, allocated once per distinct description.
///
/// The protocol requires a given identity to always mean the same description,
/// and clients compare identities to decide whether their surface already
/// matches the output — Chrome skips a conversion on a match. Handing out a
/// fresh number for every request would make two identical descriptions look
/// different, and would make the identity in `preferred_changed` disagree with
/// the one the client then gets back from `get_preferred`.
///
/// The table is capped at `KNOWN_DESCRIPTIONS` and evicts the *oldest* entry,
/// which is safe precisely because of what identities are used for. Nothing on
/// this side maps an identity back to a description — `Feedback.last` holds
/// one only to notice that the answer is unchanged — and the protocol's
/// requirement runs identity → description, not description → identity. So
/// when an evicted description is asked about again it simply gets a new
/// number, and the worst a client sees is one spurious `preferred_changed`
/// telling it to look again: an event that is merely unnecessary, never a
/// description that changed meaning under a client. The descriptions real
/// sessions live on are asked about constantly, so they re-enter the table
/// immediately and eviction lands on the mints of a hostile client instead.
fn identity_for(description: &Description) -> u32 {
    static KNOWN: Mutex<Vec<(Description, u32)>> = Mutex::new(Vec::new());
    let Ok(mut known) = KNOWN.lock() else {
        return next_identity();
    };
    if let Some((_, identity)) = known.iter().find(|(held, _)| held == description) {
        return *identity;
    }
    let identity = next_identity();
    if known.len() >= KNOWN_DESCRIPTIONS {
        known.remove(0);
    }
    known.push((*description, identity));
    identity
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
    object.ready(identity_for(&description));
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

impl Dispatch<WpColorManagementOutputV1, WlOutput> for ViewportState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _object: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        wl_output: &WlOutput,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
                let output = Output::from_resource(wl_output);
                let description = output_description(state, output.as_ref());
                send_description(image_description, description, data_init);
            }
            wp_color_management_output_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, WlSurface> for ViewportState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _object: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        surface: &WlSurface,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_color_management_surface_feedback_v1::Request;
        match request {
            // What the compositor would prefer this surface were in, which is
            // what it renders the output the surface is on into.
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                let output = state.output_of_surface(surface);
                let description = output_description(state, output.as_ref());
                send_description(image_description, description, data_init);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl ViewportState {
    /// The output a surface is being shown on, if it is on one.
    ///
    /// Walks to the root the way `mark_dirty_for_surface` does: a client asks
    /// about the subsurface it puts video in, and only the toplevel above it
    /// has been placed on an output.
    ///
    /// A window wide enough to cross a monitor edge is on *both*, so "the"
    /// output is the one it is most on. Taking whichever came first instead
    /// answers with an SDR screen for a window that is nine tenths on the HDR
    /// one, and the answer flips depending on the order outputs happen to sit
    /// in — which is not something a client can be expected to work around.
    fn output_of_surface(&self, surface: &WlSurface) -> Option<Output> {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        let view = self.views.find_by_surface(&root)?;
        let geometry = self.space.element_geometry(&view.window)?;
        self.space
            .outputs_for_element(&view.window)
            .into_iter()
            .max_by_key(|output| {
                self.space
                    .output_geometry(output)
                    .and_then(|area| area.intersection(geometry))
                    // i64, because a 4K width times a 4K height overflows the
                    // i32 these are measured in.
                    .map(|shared| shared.size.w as i64 * shared.size.h as i64)
                    .unwrap_or(0)
            })
    }

    /// Tell each surface holding feedback whether the colour it should draw in
    /// has changed.
    ///
    /// Moving a window from an SDR screen to an HDR one changes the answer to
    /// `get_preferred` without any output itself changing, so
    /// `notify_output_colour` never fires and a client that asked once would
    /// go on drawing for the screen it started on. Cheap to run on every
    /// layout: the list holds one entry per surface that asked, which is the
    /// video players and nothing else.
    pub fn notify_surface_colour(&mut self) {
        self.color_management.reap();

        let current: Vec<(usize, u32)> = self
            .color_management
            .feedback
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let output = self.output_of_surface(&entry.surface);
                let description = output_description(self, output.as_ref());
                (index, identity_for(&description))
            })
            .collect();

        for (index, identity) in current {
            let Some(entry) = self.color_management.feedback.get_mut(index) else {
                continue;
            };
            if entry.last == Some(identity) {
                continue;
            }
            entry.last = Some(identity);
            entry.object.preferred_changed(identity);
        }
    }

    /// Tell everyone holding a colour-management object that an output's
    /// colour changed.
    ///
    /// Image descriptions are immutable, so there is nothing to update: the
    /// event only says "ask again". Without it a client keeps the description
    /// it fetched at startup for as long as it runs, and `Mod4+Shift+p` moves
    /// the screen into a colour space no client is aware of.
    pub fn notify_output_colour(&mut self, name: &str) {
        self.color_management.reap();

        let outputs: Vec<_> = self
            .color_management
            .outputs
            .iter()
            .filter(|(_, wl_output)| {
                Output::from_resource(wl_output).is_some_and(|output| output.name() == name)
            })
            .map(|(object, wl_output)| (object.clone(), wl_output.clone()))
            .collect();
        for (object, wl_output) in outputs {
            object.image_description_changed();
            // The protocol asks for it, and a client that batches on
            // `wl_output.done` never applies the change without one.
            if wl_output.version() >= 2 {
                wl_output.done();
            }
        }

        // Feedback is per surface, and the surfaces that care are the ones on
        // this output — which is what re-evaluating them all works out, while
        // staying silent for the rest.
        self.notify_surface_colour();
    }
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
    fn the_luminances_go_out_in_the_order_the_protocol_takes_them() {
        // min, max, reference — not min, reference, max. The last two are the
        // same number for every SDR description, so the wrong order is
        // invisible until something is HDR and then says its reference white
        // is 10,000 cd/m².
        let (min, max, reference) = luminance_args(&hdr_description());
        assert_eq!(min, (MIN_LUMINANCE * 10_000.0) as u32);
        assert_eq!(max, PQ_PEAK as u32);
        assert_eq!(reference, hdr_description().reference_luminance as u32);
        assert!(
            reference < max,
            "reference white above the encodable maximum"
        );

        // And the target pair: minimum then maximum, the panel rather than the
        // encoding.
        let (target_min, target_max) = target_luminance_args(&hdr_description());
        assert_eq!(target_min, min);
        assert_eq!(target_max, HDR_PEAK_LUMINANCE as u32);
    }

    #[test]
    fn the_minimum_luminance_survives_the_wire_units() {
        // Ten-thousandths, not thousandths and not whole cd/m². Getting this
        // wrong moves the black point a client is told about by a factor of ten,
        // and it is the divisor in the rescale above.
        assert_eq!((MIN_LUMINANCE * 10_000.0) as u32, 2_000);
    }

    #[test]
    fn an_hdr_output_leaves_a_client_headroom_to_aim_at() {
        // The other half of the test above, and the bug this pair exists for:
        // an output already switched into HDR reported the SDR numbers, so
        // every client did this arithmetic and concluded there was no headroom
        // — Chrome then offered the page an SDR display and played the SDR
        // rendition of an HDR video on a screen that was in HDR.
        const SDR_WHITE: f32 = 203.0;
        const HDR_BLACK: f32 = 0.005;

        let description = hdr_description();
        let min = MIN_LUMINANCE;
        let reference = description.reference_luminance;
        let target_max = displayable_peak(&description);

        let scale = (SDR_WHITE - HDR_BLACK) / (reference - min);
        let rescaled = (target_max - min) * scale + HDR_BLACK;

        assert!(
            rescaled > SDR_WHITE,
            "an HDR output rescaled to {rescaled}, which a client reads as no headroom"
        );
    }

    #[test]
    fn an_hdr_output_is_described_as_what_it_is_driven_as() {
        // These three are what `udev::render` hands the renderer for an output
        // in HDR. A client told anything else is drawing for a screen that is
        // not the one in front of it.
        let description = hdr_description();
        assert_eq!(description.transfer, TransferFunction::Pq);
        assert_eq!(description.primaries, Primaries::BT2020);
        assert_eq!(
            named_transfer(description.transfer),
            Some(WireTransferFunction::St2084Pq)
        );
        assert_eq!(
            named_primaries(&description.primaries),
            Some(WirePrimaries::Bt2020)
        );
    }

    #[test]
    fn pq_encodes_to_ten_thousand_however_bright_the_panel_is() {
        // The encodable maximum is fixed by ST 2084; the displayable one is
        // the panel. Sending the panel's figure as the encoding maximum would
        // tell a client its PQ values are clipped at 1000, which they are not.
        let hdr = hdr_description();
        assert_eq!(encodable_peak(&hdr), 10_000.0);
        assert_eq!(displayable_peak(&hdr), HDR_PEAK_LUMINANCE);

        // SDR has no separate volume: reference white is both.
        let sdr = Description::default();
        assert_eq!(encodable_peak(&sdr), sdr.reference_luminance);
        assert_eq!(displayable_peak(&sdr), sdr.reference_luminance);
    }

    #[test]
    fn a_window_across_two_screens_belongs_to_the_one_it_is_most_on() {
        // The rule `output_of_surface` applies, on the geometry rather than
        // through the compositor: a window wide enough to cross a monitor edge
        // overlaps both, and the first one listed is not an answer — it flips
        // with output order and calls a window nine tenths on the HDR screen
        // an SDR one.
        use smithay::utils::{Physical, Rectangle};
        let dp1: Rectangle<i32, Physical> = Rectangle::new((0, 0).into(), (2560, 1440).into());
        let dp3: Rectangle<i32, Physical> = Rectangle::new((2560, 0).into(), (2560, 1440).into());

        let overlap = |window: Rectangle<i32, Physical>, screen: Rectangle<i32, Physical>| {
            screen
                .intersection(window)
                .map(|shared| shared.size.w as i64 * shared.size.h as i64)
                .unwrap_or(0)
        };

        // Mostly on the right-hand screen: 560 wide on one, 840 on the other.
        let straddling: Rectangle<i32, Physical> =
            Rectangle::new((2000, 100).into(), (1400, 800).into());
        assert!(overlap(straddling, dp3) > overlap(straddling, dp1));

        // Entirely on the left: the other screen must not be the answer at all.
        let left: Rectangle<i32, Physical> = Rectangle::new((100, 100).into(), (800, 600).into());
        assert!(overlap(left, dp1) > 0);
        assert_eq!(overlap(left, dp3), 0);
    }

    #[test]
    fn the_same_description_always_has_the_same_identity() {
        // A client compares identities to decide whether its surface already
        // matches the output. A fresh number per request makes every match
        // look like a mismatch, and makes the identity announced by
        // `preferred_changed` disagree with the one handed back afterwards.
        assert_eq!(
            identity_for(&Description::default()),
            identity_for(&Description::default())
        );
        assert_eq!(
            identity_for(&hdr_description()),
            identity_for(&hdr_description())
        );
        assert_ne!(
            identity_for(&Description::default()),
            identity_for(&hdr_description())
        );
    }

    #[test]
    fn a_surface_that_said_nothing_is_srgb() {
        // Not a guess: the protocol requires it.
        assert_eq!(Description::default().transfer, TransferFunction::Srgb);
        assert_eq!(Description::default().primaries, Primaries::SRGB);
    }
}
