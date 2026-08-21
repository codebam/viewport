// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-gamma-control-v1: night light, and colour calibration.
//
// This is what wlsunset, gammastep and redshift speak. There is no other way
// for them to work: a colour temperature shift is a per-output gamma ramp
// programmed into the CRTC, and a client cannot touch DRM. Without the
// protocol they start, find nothing, and exit — which is what "the screen never
// warms up in the evening" looks like from the outside.
//
// Smithay implements none of it, so the dispatch is here.
//
// The ramp itself goes through the legacy gamma ioctl rather than the atomic
// GAMMA_LUT property. Both exist on a modern driver, and the legacy one is a
// single call that does not have to join the commit that is putting a frame on
// screen — a gamma change that had to wait for a page flip would be a colour
// shift that only lands when something moves.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

/// One output's ramp: red, green and blue, each `size` entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Ramp {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

/// Split what a client wrote into three channels.
///
/// The client sends `3 * size` unsigned shorts in the machine's own byte order
/// — red, then green, then blue — through a file descriptor. Anything shorter
/// is refused rather than padded: a ramp that is half zeroes is a black screen,
/// and a client that miscounted would have no way to tell what happened.
pub fn split(bytes: &[u8], size: usize) -> Option<Ramp> {
    if size == 0 || bytes.len() != size * 3 * 2 {
        return None;
    }
    let values: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect();
    Some(Ramp {
        red: values[..size].to_vec(),
        green: values[size..size * 2].to_vec(),
        blue: values[size * 2..].to_vec(),
    })
}

/// The ramp that changes nothing: each entry the fraction of full scale its
/// index is.
///
/// What an output should wear when no client is looking after it.
pub fn identity(size: usize) -> Ramp {
    let channel: Vec<u16> = (0..size)
        .map(|i| {
            if size <= 1 {
                u16::MAX
            } else {
                // Rounded through u32 rather than computed in u16, which would
                // overflow at the top of the ramp and wrap to black.
                ((i as u32 * u16::MAX as u32) / (size as u32 - 1)) as u16
            }
        })
        .collect();
    Ramp {
        red: channel.clone(),
        green: channel.clone(),
        blue: channel,
    }
}

/// What the compositor has to be able to do for a ramp to reach a monitor.
pub trait GammaControlHandler {
    fn gamma_control_state(&mut self) -> &mut GammaControlState;

    /// How many entries this output's CRTC takes, or `None` if it cannot do
    /// gamma at all.
    fn gamma_size(&mut self, output: &Output) -> Option<u32>;

    /// Program a ramp, or restore the identity ramp when there is none.
    ///
    /// `None` means the client went away and the display should go back to
    /// what it was: a night-light client being killed must not leave the screen
    /// orange until the next reboot.
    fn set_gamma(&mut self, output: &Output, ramp: Option<&Ramp>) -> bool;
}

/// The global, and who holds each output.
#[derive(Debug, Default)]
pub struct GammaControlState {
    /// One client at a time per output. A second wlsunset started by mistake
    /// would otherwise fight the first, one ramp per second.
    controls: HashMap<String, ZwlrGammaControlV1>,
}

impl GammaControlState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrGammaControlManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrGammaControlManagerV1, _>(1, ());
        Self::default()
    }
}

/// What a control object knows.
#[derive(Debug)]
pub struct ControlData {
    pub output: Output,
    /// Whether this control has been failed already, so a destroy does not
    /// clear a ramp that belongs to whoever took over.
    ///
    /// An `AtomicBool` rather than a `Mutex` because there is nothing to
    /// protect — every touch happens on the event loop's own thread, between
    /// dispatches — and a poisoned lock here would turn one panic anywhere
    /// into every later SetGamma and destroy panicking in its place.
    failed: Arc<AtomicBool>,
}

impl<D> GlobalDispatch<ZwlrGammaControlManagerV1, (), D> for GammaControlState
where
    D: GlobalDispatch<ZwlrGammaControlManagerV1, ()>
        + Dispatch<ZwlrGammaControlManagerV1, ()>
        + Dispatch<ZwlrGammaControlV1, ControlData>
        + GammaControlHandler
        + 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<ZwlrGammaControlManagerV1, (), D> for GammaControlState
where
    D: Dispatch<ZwlrGammaControlManagerV1, ()>
        + Dispatch<ZwlrGammaControlV1, ControlData>
        + GammaControlHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };

        let Some(output) = Output::from_resource(&output) else {
            // The output went between the client looking it up and asking.
            // Initialising and failing is the only way to say so: there is no
            // error on the manager for it.
            let control = data_init.init(
                id,
                ControlData {
                    output: Output::new(
                        "gone".to_owned(),
                        smithay::output::PhysicalProperties {
                            size: (0, 0).into(),
                            subpixel: smithay::output::Subpixel::Unknown,
                            make: String::new(),
                            model: String::new(),
                            serial_number: String::new(),
                        },
                    ),
                    failed: Arc::new(AtomicBool::new(true)),
                },
            );
            control.failed();
            return;
        };

        let name = output.name();
        let size = state.gamma_size(&output);
        let control = data_init.init(
            id,
            ControlData {
                output,
                failed: Arc::new(AtomicBool::new(size.is_none())),
            },
        );

        let Some(size) = size else {
            // No gamma on this CRTC — a virtual output, or a driver that does
            // not offer it. Saying so lets a night-light client skip this
            // monitor rather than wait for a ramp that will never take.
            control.failed();
            return;
        };

        // How many entries the client has to send. Everything it does next
        // depends on this number, so it goes out before anything else can.
        control.gamma_size(size);

        // The new client takes over, and the old one is told rather than left
        // writing ramps that go nowhere.
        if let Some(previous) = state
            .gamma_control_state()
            .controls
            .insert(name, control.clone())
        {
            if let Some(data) = previous.data::<ControlData>() {
                data.failed.store(true, Ordering::Relaxed);
            }
            previous.failed();
        }
    }
}

impl<D> Dispatch<ZwlrGammaControlV1, ControlData, D> for GammaControlState
where
    D: Dispatch<ZwlrGammaControlV1, ControlData> + GammaControlHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        control: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };

        if data.failed.load(Ordering::Relaxed) {
            // Someone else owns this output now. Silently ignoring is right:
            // the client has already been told, and a second failed event on
            // an object it is about to destroy is noise.
            return;
        }

        let Some(size) = state.gamma_size(&data.output) else {
            control.failed();
            return;
        };

        let wanted = size as usize * 3 * 2;
        let bytes = match read_exactly(&fd, wanted) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("could not read a gamma ramp: {e}");
                control.failed();
                return;
            }
        };

        let Some(ramp) = split(&bytes, size as usize) else {
            control.post_error(
                zwlr_gamma_control_v1::Error::InvalidGamma,
                "the ramp is not three channels of the size this output asked for",
            );
            return;
        };

        if !state.set_gamma(&data.output, Some(&ramp)) {
            control.failed();
        }
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        control: &ZwlrGammaControlV1,
        data: &ControlData,
    ) {
        if data.failed.load(Ordering::Relaxed) {
            // Superseded: the ramp on screen belongs to whoever took over, and
            // clearing it here would undo their work.
            return;
        }
        let name = data.output.name();
        let managed = state.gamma_control_state();
        if managed.controls.get(&name) == Some(control) {
            managed.controls.remove(&name);
            // Back to the identity ramp. A night-light client that was killed
            // must not leave the screen orange until the next reboot.
            state.set_gamma(&data.output, None);
        }
    }
}

/// Read exactly `len` bytes, or say why not.
///
/// A client writes the ramp into a memfd and hands the descriptor over, so a
/// short read is a client that miscounted rather than a stream that will have
/// more later — but read(2) is still allowed to return less than asked for, so
/// this loops rather than trusting one call.
///
/// The duplicate is made nonblocking first. The descriptor arrives over the
/// wire from an untrusted client, and nothing on the wire enforces the memfd:
/// a hostile one may send a pipe with no writer, and a blocking read on that
/// would hold the single-threaded event loop still — every output frozen — for
/// as long as the silence lasted. Nonblocking turns the silence into an error
/// this side survives. For the memfd the protocol actually expects it changes
/// nothing: O_NONBLOCK has no effect on reads from a regular file.
fn read_exactly(fd: &OwnedFd, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut file = std::fs::File::from(fd.as_fd().try_clone_to_owned()?);

    // The flag lives on the open file description, which a dup shares with the
    // client's own descriptor — so strictly speaking the client's copy turns
    // nonblocking too. That is harmless: a memfd ignores it, and a pipe the
    // client kept open was not going to be written again once the ramp was
    // sent. If the flags cannot even be read, carrying on as before is better
    // than failing a ramp that would have worked.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags != -1 {
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }

    // Loop only while progress is being made. On a regular file every read
    // returns everything left, so this runs once; on anything else, a read
    // that comes back empty or WouldBlock means the rest is not coming.
    let mut bytes = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the ramp did not arrive",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the ramp did not arrive",
                ));
            }
            Err(e) => return Err(e),
        }
    }

    // Nothing may follow. A descriptor holding more than the ramp is a client
    // that has miscounted, and taking the first part of it would program a
    // ramp made of the wrong numbers.
    let mut extra = [0u8; 1];
    match file.read(&mut extra) {
        Ok(0) => Ok(bytes),
        // A pipe whose writer is still open but has said everything it had to
        // say reports WouldBlock here rather than end of file. That is
        // "nothing more arrived", which for a ramp of exactly the right length
        // is success; refusing it would punish a client for holding its own
        // descriptor open.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(bytes),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the ramp is longer than this output's gamma size",
        )),
        Err(e) => Err(e),
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_gamma_control {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1: ()
        ] => $crate::gamma::GammaControlState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1: ()
        ] => $crate::gamma::GammaControlState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1: $crate::gamma::ControlData
        ] => $crate::gamma::GammaControlState);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    #[test]
    fn the_three_channels_come_out_in_order() {
        // Red, then green, then blue — not interleaved. Getting this wrong
        // produces a picture that is wrong in a way that looks deliberate.
        let ramp = split(&bytes_of(&[1, 2, 3, 4, 5, 6]), 2).expect("should split");
        assert_eq!(ramp.red, vec![1, 2]);
        assert_eq!(ramp.green, vec![3, 4]);
        assert_eq!(ramp.blue, vec![5, 6]);
    }

    #[test]
    fn a_short_ramp_is_refused_rather_than_padded() {
        // Padding with zeroes is a black screen, and the client would have no
        // way to tell what happened.
        assert!(split(&bytes_of(&[1, 2, 3, 4, 5]), 2).is_none());
        assert!(split(&[], 2).is_none());
    }

    #[test]
    fn a_long_ramp_is_refused_too() {
        // Taking the first part of it programs a ramp made of the wrong
        // numbers, which is worse than refusing.
        assert!(split(&bytes_of(&[1, 2, 3, 4, 5, 6, 7]), 2).is_none());
    }

    #[test]
    fn a_zero_size_is_not_a_ramp() {
        assert!(split(&[], 0).is_none());
    }

    #[test]
    fn the_identity_ramp_runs_from_nothing_to_everything() {
        // Computed in u16 this overflows at the top and wraps to black, which
        // is a screen that goes dark when a night-light client exits.
        let ramp = identity(256);
        assert_eq!(ramp.red[0], 0);
        assert_eq!(ramp.red[255], u16::MAX);
        assert_eq!(ramp.red[128], 32896);
        assert!(ramp.red.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(ramp.red, ramp.blue);
    }

    #[test]
    fn a_one_entry_ramp_does_not_divide_by_zero() {
        let ramp = identity(1);
        assert_eq!(ramp.red, vec![u16::MAX]);
    }

    #[test]
    fn a_full_size_ramp_splits() {
        // What wlsunset actually sends: 256 entries per channel.
        let values: Vec<u16> = (0..256 * 3).map(|i| i as u16).collect();
        let ramp = split(&bytes_of(&values), 256).expect("should split");
        assert_eq!(ramp.red.len(), 256);
        assert_eq!(ramp.green[0], 256);
        assert_eq!(ramp.blue[255], 767);
    }
}
