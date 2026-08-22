// SPDX-License-Identifier: GPL-3.0-or-later
//
// More than one graphics card in the machine, and the questions that only have
// an answer when there is.
//
// The backend in `udev.rs` opens every card on the seat, gives each one its own
// renderer and its own output manager, and draws each screen with the card that
// scans it out. That much is mechanical. What is not mechanical is what happens
// where the cards *meet*, and this is where those decisions live — on their own,
// out of the DRM path, because every one of them is a rule that can be stated
// and checked without a graphics card in the machine.
//
// There are four of them.
//
//   * **Two cards, one connector name.** Connector names are handed out per
//     card, so an Intel display controller and the discrete card beside it both
//     have a `DP-1`. Everything that names a screen — the config file's
//     per-output rules, `active_output`, the saved layout, `wl_output`'s name,
//     the vblank bookkeeping keyed by name — takes that name as unique, and on
//     a two-card laptop it silently is not. See [`unique_output_name`].
//
//   * **A buffer only one card can read.** A client allocates on the card its
//     dmabuf feedback named, which is the card its window is being shown on. It
//     is not necessarily the card that has to import it a moment later, because
//     windows move between screens. See [`Reach`], and the note on the fallback
//     below.
//
//   * **What clients are told to allocate against.** The default advertisement
//     names one card, because the protocol's default tranche is one device.
//     Which card, and whether the format list should be cut down to what every
//     card can read, is a preference: see [`CrossGpu`].
//
//   * **Saying it once.** A buffer that cannot cross is a per-frame condition
//     and a once-per-session fact. See [`Reported`].
//
// ## What happens to a buffer the scanout card cannot import
//
// It is dropped from that screen: the surface does not appear there, and every
// other screen still shows it. That is the decision, and it is the same one the
// code made before this module existed — the difference is that it is now said
// out loud, once, naming the format, the modifier and the two cards, instead of
// being a window that is mysteriously missing from one monitor.
//
// The alternative is a copy: import the buffer on the card that allocated it,
// blit it into a buffer allocated on the scanout card with a modifier both of
// them understand, and sample that. It is what most compositors do and it is
// not free — a full-surface copy across PCIe every frame, per surface, per
// screen it is foreign to. It is deliberately not implemented here, and the
// reasons are worth writing down so the next person does not rediscover them:
//
//   * The render path is generic over one renderer at a time (`render_pass` in
//     `udev.rs` is compiled once for Vulkan and once for GLES). A copy needs
//     two renderers live in the same pass, which is a different shape.
//   * Smithay's own multi-GPU renderer is built on its GLES `GraphicsApi`, and
//     this compositor's renderer is Vulkan wherever Vulkan works — the whole
//     colour-management and explicit-sync path depends on it.
//   * Two of the three mitigations below make the copy unnecessary for a client
//     that follows the protocol, and the protocol is how this is *supposed* to
//     be solved: per-surface dmabuf feedback exists precisely so a client
//     reallocates when its window moves to another card.
//
// So, in order of what actually fixes it:
//
//   1. Per-surface feedback names the card the window is being shown on. A
//      client that honours it — everything on Mesa does — reallocates and the
//      question never arises. This is on always.
//   2. `cross_gpu = "portable"` cuts the *default* advertisement down to the
//      formats every card on the seat can import, which fixes the client that
//      ignores per-surface feedback, at the cost of the modifiers only one card
//      understands. Off by default, because that cost is paid by every client
//      on a machine where most windows never leave the card they were
//      allocated on.
//   3. Failing both, the surface is missing from that one screen and the log
//      says so.

use std::collections::HashSet;

use smithay::backend::allocator::Format;

/// A screen's name, made unique across the cards.
///
/// Connector names are per card: `DP-1` on the integrated display controller
/// and `DP-1` on the discrete card beside it are two different monitors with
/// one name. Nothing downstream is prepared for that. `output_config` is keyed
/// by name and would apply one monitor's mode to another; `active_output` is a
/// name and would stop identifying a single screen; the saved layout is by name
/// and would restore one screen's position onto both; `last_vblank_by_output`
/// is keyed by name and would let one screen's flip silence the other's fifo
/// barriers, which is the bug `barrier_ticks_deferred_awake` was added to catch
/// on a two-*monitor* desk and would now be reachable on a two-*card* one.
///
/// The plain connector name wherever it is free, which is every screen on a
/// single-GPU machine and the first card's screens on any machine — so nothing
/// that exists today is renamed, and no config file stops matching. Only the
/// collision is suffixed, and it is suffixed with the card index rather than
/// with anything read off the hardware: a card's minor number moves between
/// boots and its bus address is too long to type into a config file, while the
/// index is at least stable for as long as the seat enumerates its cards the
/// same way.
///
/// A name is never reused once handed out, which is why `taken` is the live
/// output names rather than a per-scan set: a monitor that keeps its name for
/// the life of a client is worth more than one that is prettier after an
/// unplug.
pub fn unique_output_name(base: &str, device: usize, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    let suffixed = format!("{base}-gpu{device}");
    if !taken.contains(&suffixed) {
        return suffixed;
    }
    // Two cards at the same index cannot happen, so reaching here means the
    // name was already taken by something this function did not hand out.
    // Counting up rather than returning a duplicate: a duplicate is the bug
    // this exists to prevent, and an ugly name is not.
    for n in 2.. {
        let candidate = format!("{base}-gpu{device}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("the loop above only ends by returning")
}

/// Which cards can read one client buffer.
///
/// A dmabuf is handed over once and shown for as long as the client keeps it,
/// possibly on more than one screen and possibly on screens belonging to
/// different cards. Whether a given card can import it is a property of the
/// buffer's modifier and that card's driver, and the only way to find out is to
/// try — so the answer is collected once, when the buffer arrives, rather than
/// per frame.
///
/// Three outcomes matter and they are not the same thing:
///
///   * every card takes it — ordinary, and the single-GPU case by definition;
///   * some do and some do not — the buffer is usable, the client is told so,
///     and its window will be missing from the screens on the cards that
///     refused. This is the one worth a word in the log.
///   * none does — the buffer is unusable and the client has to be told, which
///     is what `linux-dmabuf`'s import notifier is for. Telling it anything
///     else means a window that never appears anywhere, with no error.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reach {
    /// Cards, by index into `Udev::devices`, whose renderer imported it.
    pub taken_by: Vec<usize>,
    /// Cards whose renderer refused it.
    pub refused_by: Vec<usize>,
}

impl Reach {
    /// Sort the per-card answers into the two lists.
    pub fn of(results: impl IntoIterator<Item = (usize, bool)>) -> Self {
        let mut reach = Reach::default();
        for (device, imported) in results {
            if imported {
                reach.taken_by.push(device);
            } else {
                reach.refused_by.push(device);
            }
        }
        reach
    }

    /// Whether any card at all can show this buffer.
    ///
    /// The question `dmabuf_imported` has to answer. Not "can the primary" —
    /// that was the old answer and it is wrong the moment there are two cards,
    /// because per-surface feedback tells a client on the second card's monitor
    /// to allocate for the second card, and the second card's buffer is then
    /// rejected by the first card's renderer and the client is killed for
    /// having done exactly what it was told.
    pub fn usable(&self) -> bool {
        !self.taken_by.is_empty()
    }

    /// Whether this buffer works on some cards and not others.
    ///
    /// The one state that is neither fine nor fatal: the client keeps its
    /// buffer, and a window carrying it is missing from some monitors.
    pub fn split(&self) -> bool {
        !self.taken_by.is_empty() && !self.refused_by.is_empty()
    }
}

/// What the default dmabuf advertisement says on a seat with more than one card.
///
/// The `linux-dmabuf` default tranche names exactly one device, so on a machine
/// with two cards something has to be chosen. Both answers are defensible and
/// which is right depends on the desk, so it is a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossGpu {
    /// Name the primary card and everything its renderer can import.
    ///
    /// The fast answer and the default. A window that stays on the card it was
    /// allocated for — which is most windows, because the shell opens them on
    /// the active screen and most people leave them there — gets every modifier
    /// its card understands, including the tiled and compressed ones that are
    /// the whole reason modifiers exist.
    #[default]
    Native,
    /// Name the primary card, but only the formats *every* card can import.
    ///
    /// For the desk where windows are dragged between two cards' monitors all
    /// day, and for the client that ignores per-surface feedback and so never
    /// reallocates when its window moves. The cost is real and is paid by every
    /// client whether or not its window ever moves: what survives the
    /// intersection is usually linear and whatever the two drivers happen to
    /// share, so a fullscreen window may lose framebuffer compression.
    Portable,
}

/// What `cross_gpu`, `--cross-gpu` and `$VIEWPORT_CROSS_GPU` accept.
///
/// Read once, when the cards are opened. A card that arrives mid-session — an
/// eGPU, or one coming back from a bus reset on a different node — does not
/// re-narrow the default advertisement, because the `linux-dmabuf` global
/// carries its default feedback from the moment it is created and changing it
/// means destroying the global and making another, which every client bound to
/// it would see as the protocol going away. The per-surface feedback is sent
/// per frame and does follow the new card, so a client that honours it is
/// unaffected; the one that does not gets the advertisement the session started
/// with. Worth knowing before concluding that `portable` did nothing.
///
/// Trimmed and case-folded, the same forgiveness `pixel_format` gets. An
/// unknown value is an error rather than a guess, for the reason every other
/// setting in this tree gives: a typo that silently means the default leaves
/// somebody certain the setting does nothing.
pub fn parse_cross_gpu(value: &str) -> anyhow::Result<CrossGpu> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "native" | "auto" | "default" => Ok(CrossGpu::Native),
        "portable" | "shared" | "common" => Ok(CrossGpu::Portable),
        other => Err(anyhow::anyhow!(
            "{other:?} is not a cross-GPU mode; it is \"native\" or \"portable\""
        )),
    }
}

/// The formats every card on the seat can import.
///
/// Order is the first card's, because order is preference: a format list is
/// consumed in the order it is given and the primary is the card most clients
/// will actually allocate against. Sorting or hashing to intersect faster would
/// throw that away, and the lists are hundreds of entries at most against two
/// or three cards.
///
/// An empty result is possible in principle — two drivers with no modifier in
/// common — and is handled by the caller rather than papered over here: a
/// client told about no formats at all cannot allocate anything, which is worse
/// than a client told about formats one of its screens cannot read.
pub fn shared_formats(per_device: &[Vec<Format>]) -> Vec<Format> {
    let Some((first, rest)) = per_device.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .filter(|format| rest.iter().all(|other| other.contains(format)))
        .copied()
        .collect()
}

/// The format list the default advertisement should carry.
///
/// Falls back to the primary's own list when the intersection is empty, with
/// the warning left to the caller: no formats at all is not a session anyone
/// can use, and being wrong about one card's monitors is recoverable while
/// advertising nothing is not.
pub fn default_formats(policy: CrossGpu, per_device: &[Vec<Format>]) -> Vec<Format> {
    let primary = per_device.first().cloned().unwrap_or_default();
    match policy {
        CrossGpu::Native => primary,
        CrossGpu::Portable => {
            let shared = shared_formats(per_device);
            if shared.is_empty() {
                primary
            } else {
                shared
            }
        }
    }
}

/// What has already been said, so it is not said again every frame.
///
/// A buffer that one card cannot import is a fact about a format, a modifier
/// and a card — not about the frame it was noticed in. The condition holds for
/// as long as the client keeps allocating that way, which is the whole session,
/// and a client painting at 240Hz would otherwise write four log lines a
/// millisecond about a window that is merely missing from one monitor.
///
/// Keyed by what identifies the case rather than by the buffer, so a client
/// that cycles through a swapchain of four buffers says it once and not four
/// times.
#[derive(Debug, Default)]
pub struct Reported(HashSet<(u32, u64, usize)>);

impl Reported {
    /// Whether this is the first time this format, modifier and card have come
    /// up. Records it either way.
    pub fn first_time(&mut self, code: u32, modifier: u64, device: usize) -> bool {
        self.0.insert((code, modifier, device))
    }

    /// How many distinct cases have been reported. Diagnostics only.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::{Fourcc, Modifier};

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn one_card_keeps_the_connector_name() {
        // The single-GPU path, which is every current user: a screen is called
        // what the kernel calls its connector and nothing this module does may
        // change that.
        assert_eq!(unique_output_name("DP-1", 0, &names(&[])), "DP-1");
        assert_eq!(
            unique_output_name("eDP-1", 0, &names(&["DP-1", "HDMI-A-1"])),
            "eDP-1"
        );
    }

    #[test]
    fn a_second_card_with_the_same_connector_name_is_told_apart() {
        assert_eq!(
            unique_output_name("DP-1", 1, &names(&["DP-1"])),
            "DP-1-gpu1"
        );
    }

    #[test]
    fn the_card_that_got_there_first_keeps_the_plain_name() {
        // Which matters after an unplug: renaming the surviving screen because
        // its neighbour went away would break every client holding its name.
        let taken = names(&["DP-1"]);
        assert_eq!(unique_output_name("DP-1", 2, &taken), "DP-1-gpu2");
        assert_eq!(unique_output_name("DP-2", 2, &taken), "DP-2");
    }

    #[test]
    fn a_suffix_that_is_somehow_taken_does_not_produce_a_duplicate() {
        let taken = names(&["DP-1", "DP-1-gpu1"]);
        assert_eq!(unique_output_name("DP-1", 1, &taken), "DP-1-gpu1-2");
    }

    #[test]
    fn a_buffer_every_card_takes_is_usable_and_unremarkable() {
        let reach = Reach::of([(0, true), (1, true)]);
        assert!(reach.usable());
        assert!(!reach.split());
    }

    #[test]
    fn a_buffer_the_primary_refuses_is_still_usable() {
        // The bug this whole type exists for. A client on the second card's
        // monitor allocates for the second card because that is what its
        // per-surface feedback told it to do, and asking only the primary
        // rejects the buffer — which, over linux-dmabuf, kills the client.
        let reach = Reach::of([(0, false), (1, true)]);
        assert!(reach.usable());
        assert!(reach.split());
        assert_eq!(reach.taken_by, vec![1]);
        assert_eq!(reach.refused_by, vec![0]);
    }

    #[test]
    fn a_buffer_no_card_takes_has_to_be_refused() {
        // The client is waiting for an answer and there is no honest one but
        // no. Saying yes gives a window that never appears anywhere.
        let reach = Reach::of([(0, false), (1, false)]);
        assert!(!reach.usable());
        assert!(!reach.split());
    }

    #[test]
    fn no_cards_answered_at_all() {
        // Every card offline mid-recovery. Neither usable nor split, and the
        // caller has to decide — see `dmabuf_imported`, which accepts rather
        // than killing clients over a GPU that is being reset.
        let reach = Reach::of([]);
        assert!(!reach.usable());
        assert!(!reach.split());
    }

    fn format(code: Fourcc, modifier: u64) -> Format {
        Format {
            code,
            modifier: Modifier::from(modifier),
        }
    }

    #[test]
    fn one_card_shares_everything_with_itself() {
        // The single-GPU case has to come out unchanged whichever policy is
        // set, or "portable" becomes a way to break a machine it has nothing
        // to say about.
        let only = vec![format(Fourcc::Argb8888, 0), format(Fourcc::Xrgb8888, 7)];
        assert_eq!(shared_formats(std::slice::from_ref(&only)), only);
        assert_eq!(
            default_formats(CrossGpu::Portable, std::slice::from_ref(&only)),
            only
        );
        assert_eq!(
            default_formats(CrossGpu::Native, std::slice::from_ref(&only)),
            only
        );
    }

    #[test]
    fn the_shared_set_is_what_both_cards_take_in_the_primarys_order() {
        let intel = vec![
            format(Fourcc::Argb8888, 0),
            format(Fourcc::Argb8888, 0x100000000000001),
            format(Fourcc::Xrgb8888, 0),
        ];
        let discrete = vec![format(Fourcc::Xrgb8888, 0), format(Fourcc::Argb8888, 0)];
        assert_eq!(
            shared_formats(&[intel, discrete]),
            vec![format(Fourcc::Argb8888, 0), format(Fourcc::Xrgb8888, 0)]
        );
    }

    #[test]
    fn portable_narrows_and_native_does_not() {
        let intel = vec![format(Fourcc::Argb8888, 0), format(Fourcc::Argb8888, 9)];
        let discrete = vec![format(Fourcc::Argb8888, 0)];
        let sets = [intel.clone(), discrete];
        assert_eq!(default_formats(CrossGpu::Native, &sets), intel);
        assert_eq!(
            default_formats(CrossGpu::Portable, &sets),
            vec![format(Fourcc::Argb8888, 0)]
        );
    }

    #[test]
    fn two_cards_with_nothing_in_common_still_get_an_advertisement() {
        // Advertising nothing is a session where no GL or Vulkan client can
        // start at all. Being wrong about the second card's monitors is a
        // window missing from them.
        let intel = vec![format(Fourcc::Argb8888, 1)];
        let discrete = vec![format(Fourcc::Argb8888, 2)];
        assert!(shared_formats(&[intel.clone(), discrete.clone()]).is_empty());
        assert_eq!(
            default_formats(CrossGpu::Portable, &[intel.clone(), discrete]),
            intel
        );
    }

    #[test]
    fn a_cross_gpu_mode_is_read_in_any_form_somebody_types_it() {
        assert_eq!(parse_cross_gpu("portable").unwrap(), CrossGpu::Portable);
        assert_eq!(parse_cross_gpu(" PORTABLE ").unwrap(), CrossGpu::Portable);
        assert_eq!(parse_cross_gpu("native").unwrap(), CrossGpu::Native);
        assert_eq!(parse_cross_gpu("").unwrap(), CrossGpu::Native);
        assert_eq!(parse_cross_gpu("  ").unwrap(), CrossGpu::Native);
    }

    #[test]
    fn a_cross_gpu_mode_that_is_not_one_is_reported_not_ignored() {
        assert!(parse_cross_gpu("copy").is_err());
        assert!(parse_cross_gpu("yes").is_err());
    }

    #[test]
    fn a_case_is_reported_once_and_a_new_one_is_reported_again() {
        let mut reported = Reported::default();
        assert!(reported.first_time(0x34325241, 0x100000000000001, 1));
        assert!(!reported.first_time(0x34325241, 0x100000000000001, 1));
        // Same buffer, other card.
        assert!(reported.first_time(0x34325241, 0x100000000000001, 2));
        // Same card, other modifier.
        assert!(reported.first_time(0x34325241, 0, 1));
        assert_eq!(reported.len(), 3);
        assert!(!reported.is_empty());
    }
}
