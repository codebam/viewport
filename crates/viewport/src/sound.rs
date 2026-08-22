// SPDX-License-Identifier: GPL-3.0-or-later
//
// Event sounds, for notifications.
//
// The notification specification has three sound hints and this compositor
// honoured none of them, because it had no way to make a sound at all. The
// daemons it replaced — mako, dunst — are where that playback used to live,
// and claiming org.freedesktop.Notifications (see `notification.rs`) took the
// sound away along with the window.
//
// PipeWire, which this program already links for the screencast portal, and
// symphonia to decode. The obvious alternative is libcanberra, which is what
// every desktop that predates PipeWire uses and which answers `sound-name`
// out of the box — but it is another C library in the closure and another
// entry in nine AUR packages, to do two things this can do itself. What it
// would have given for free is the theme lookup, so that is written out below.
//
// Playing is asynchronous by construction: `play` decodes and streams on a
// thread of its own and returns immediately. That is not a nicety. It is
// called on the D-Bus thread while the sender blocks waiting for its
// notification id, so anything slower would be every notifying application
// stalled for the length of a bark.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::spa;

/// What to play. The two shapes are the two sound hints: a path is played as
/// given, a name is looked up in the sound theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sound {
    /// An absolute path to a sound file.
    File(String),
    /// A name from the sound naming specification, resolved against the
    /// installed theme — `"message-new-instant"`, `"bell"`.
    Name(String),
}

impl Sound {
    /// The sound a configuration file asks for, if it asks for one.
    ///
    /// A file wins over a name for the same reason it does among the hints
    /// (see `notification::sound`): the path is unambiguous and the name may
    /// resolve to nothing.
    pub fn from_config(file: Option<&str>, name: Option<&str>) -> Option<Self> {
        // Empty is absent, so that a configuration takes the sound away by
        // blanking the value rather than having to delete the key — the same
        // reading `wallpaper` gives an empty string.
        let file = file.filter(|value| !value.is_empty());
        let name = name.filter(|value| !value.is_empty());
        file.map(|value| Self::File(value.to_owned()))
            .or_else(|| name.map(|value| Self::Name(value.to_owned())))
    }

    /// The file this names, if there is one to find.
    fn resolve(&self) -> Option<PathBuf> {
        match self {
            Self::File(path) => {
                let path = PathBuf::from(path);
                path.is_file().then_some(path)
            }
            Self::Name(name) => theme::find(name),
        }
    }
}

/// Decoded audio, ready to hand to PipeWire.
///
/// Interleaved `f32` because that is what both ends already speak: symphonia
/// converts to it on the way out and `F32LE` is a format every PipeWire graph
/// accepts, so there is no conversion pass in the middle.
struct Pcm {
    rate: u32,
    channels: u32,
    /// Interleaved frames: channel 0, channel 1, channel 0, ...
    samples: Vec<f32>,
}

impl Pcm {
    fn stride(&self) -> usize {
        self.channels as usize * std::mem::size_of::<f32>()
    }

    /// What this occupies once decoded: the samples, which is everything the
    /// cache holds a reference to.
    fn bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<f32>()
    }
}

/// Most the decoded cache may hold in total, across every entry.
///
/// The sounds a theme ships are a second or two of stereo, which decodes to
/// under a megabyte of `f32` each, so 64 MiB holds every sound a theme
/// plausibly carries several times over and the cache keeps doing its job —
/// one decode per sound for the life of the session. The bound exists for the
/// pathological case rather than the typical one: the hints come off the bus,
/// where any sender may name any readable file, and an insert-only cache would
/// keep a decoded copy of each such name forever, which is a memory-exhaustion
/// DoS with a notification as the trigger.
const DECODED_BUDGET: usize = 64 * 1024 * 1024;

/// The largest sound file that will be decoded.
///
/// Thirty-two megabytes, against the 64 MiB [`DECODED_BUDGET`] the decoded
/// cache holds, because the two bound different things: the budget bounds what
/// is *kept*, this bounds what one notification makes this *read*. The path in
/// a `sound-file` hint is chosen by whoever published it, and not every
/// publisher is the application it says it is — the decode below materialises
/// the whole file before the budget ever gets a say, so a gigabyte of WAV in a
/// hint would be several gigabytes of `f32` per notification. The same
/// discipline [`crate::icon::art_data_url`] applies to cover art: the size
/// comes from the open file rather than from the name, so what is checked is
/// the inode that would be read, and a device or a FIFO named like a sound is
/// refused before anything blocks on it. A theme sound is a second or two of
/// stereo; nothing honest comes near this.
const MAX_FILE: u64 = 32 << 20;

/// Most samples one decode may produce.
///
/// The size check bounds the file, and a compressed file can still expand: an
/// hour of silence in Ogg is kilobytes on disk and gigabytes of `f32` out.
/// This is [`DECODED_BUDGET`] expressed as a sample count, checked as the
/// decode runs so that the answer stops arriving rather than arriving too big
/// to cache or to play.
const MAX_SAMPLES: usize = DECODED_BUDGET / std::mem::size_of::<f32>();

/// The decoded cache proper.
///
/// A map for the lookup and a queue recording the order entries were last
/// used in, so that when the budget runs out the entry least recently played
/// is the one evicted rather than an arbitrary one.
struct Cache {
    map: HashMap<PathBuf, Arc<Pcm>>,
    order: VecDeque<PathBuf>,
    bytes: usize,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    /// The decoded sound for a path, remembering that it was wanted.
    fn get(&mut self, path: &Path) -> Option<Arc<Pcm>> {
        let pcm = self.map.get(path)?.clone();
        self.touch(path);
        Some(pcm)
    }

    /// Move a path to the most recently used end of the queue.
    fn touch(&mut self, path: &Path) {
        if let Some(at) = self.order.iter().position(|held| held == path) {
            self.order.remove(at);
            self.order.push_back(path.to_owned());
        }
    }

    /// Put a decoded sound in, evicting least recently used entries until the
    /// total is back under the budget.
    ///
    /// A single sound larger than the whole budget is still cached — evicting
    /// everything else and stopping at an empty cache is the most the rule can
    /// take from it, and one oversized entry is a bound of its own.
    fn insert(&mut self, path: PathBuf, pcm: Arc<Pcm>) {
        let bytes = pcm.bytes();
        while self.bytes + bytes > DECODED_BUDGET {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.map.remove(&oldest) {
                self.bytes -= gone.bytes();
            }
        }

        // Replacing an entry that was already there — a decode that raced
        // another thread's — takes the old size off the books and its place
        // out of the queue before the fresh one goes in at the back.
        if let Some(gone) = self.map.insert(path.clone(), pcm) {
            self.bytes -= gone.bytes();
            if let Some(at) = self.order.iter().position(|held| *held == path) {
                self.order.remove(at);
            }
        }
        self.order.push_back(path);
        self.bytes += bytes;
    }
}

/// Plays sounds, and remembers the ones it has played.
pub struct Player {
    shared: Arc<Shared>,
}

struct Shared {
    /// Decoded files, by the path they were decoded from.
    ///
    /// The same short sound plays on every notification for the life of the
    /// session; without this each one is a file read and a Vorbis decode. This
    /// is what `canberra.cache-control` would have asked for.
    ///
    /// Bounded: entries are evicted least-recently-used once the decoded total
    /// passes `DECODED_BUDGET`, because the paths come from sound hints any
    /// bus sender chose, and a cache that grew with each of them would be a
    /// memory-exhaustion DoS wearing the costume of an optimisation.
    decoded: Mutex<Cache>,
}

impl Player {
    /// Check that there is a sound server to play through, and prepare to.
    ///
    /// `None` when there is not — a headless session, a machine with no
    /// PipeWire. That is a desktop that makes no noise, which is what it did
    /// before, so it is logged and not an error. The connection made here is
    /// only the question being asked: each sound opens its own, because each
    /// one lives on a thread of its own and dies with it.
    pub fn new() -> Option<Self> {
        pw::init();
        if let Err(e) = connect() {
            tracing::info!("no notification sounds: {e}");
            return None;
        }
        Some(Self {
            shared: Arc::new(Shared {
                decoded: Mutex::new(Cache::new()),
            }),
        })
    }

    /// Start a sound and return.
    ///
    /// Everything — resolving the name, reading the file, decoding it, and the
    /// PipeWire loop that plays it — happens on a thread this spawns. See the
    /// module comment for why none of it may happen here.
    pub fn play(&self, sound: &Sound) {
        let shared = self.shared.clone();
        let sound = sound.clone();
        let spawned = std::thread::Builder::new()
            .name("viewport-sound".to_owned())
            .spawn(move || {
                if let Err(e) = shared.run(&sound) {
                    tracing::warn!("could not play {sound:?}: {e}");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not start a sound thread: {e}");
        }
    }
}

impl Shared {
    fn run(&self, sound: &Sound) -> anyhow::Result<()> {
        let path = sound
            .resolve()
            .ok_or_else(|| anyhow::anyhow!("no such sound"))?;

        // Under the lock only to look and to record, never across the decode:
        // two notifications at once would otherwise be one of them waiting for
        // the other's file to be read.
        let cached = self.decoded.lock().ok().and_then(|mut d| d.get(&path));
        let pcm = match cached {
            Some(pcm) => pcm,
            None => {
                let pcm = Arc::new(decode(&path)?);
                if let Ok(mut decoded) = self.decoded.lock() {
                    decoded.insert(path.clone(), pcm.clone());
                }
                pcm
            }
        };

        stream(&path, &pcm)
    }
}

/// Open a PipeWire connection, and close it again.
///
/// Only to find out whether one can be opened. A loop of its own rather than
/// the compositor's: this runs before the event loop exists.
fn connect() -> anyhow::Result<()> {
    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow::anyhow!("creating a pipewire loop: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| anyhow::anyhow!("creating a pipewire context: {e}"))?;
    context
        .connect_rc(None)
        .map_err(|e| anyhow::anyhow!("connecting to pipewire: {e}"))?;
    Ok(())
}

/// How long a stream may sit unrouted before the loop is shut down under it.
///
/// A notification sound is a second or two; the drain that normally ends the
/// loop fires within moments of the last buffer, so thirty seconds is an order
/// of magnitude past anything real. The point of the bound is the case where
/// the drain never comes at all — a headset yanked mid-play, a session manager
/// restarted out from under the stream leaves AUTOCONNECT pending forever,
/// and without this each such notification would leave a thread and a
/// PipeWire connection behind it for the rest of the session.
const ROUTING_GRACE: Duration = Duration::from_secs(30);

/// Play decoded audio through, and return when it has finished.
///
/// The stream is described in the file's own rate and channel count rather
/// than resampled to the graph's. PipeWire converts, and it is better at it
/// than anything worth writing here.
///
/// `path` names the file only so the timeout below can say what went unheard.
fn stream(path: &Path, pcm: &Arc<Pcm>) -> anyhow::Result<()> {
    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow::anyhow!("creating a pipewire loop: {e}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| anyhow::anyhow!("creating a pipewire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow::anyhow!("connecting to pipewire: {e}"))?;

    // MEDIA_ROLE "Notification" is what tells the session manager this is an
    // alert and not music: it is the property a "duck other streams while a
    // notification plays" policy matches on, and the reason this is worth
    // setting even though nothing here depends on it.
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "viewport-notification",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Notification",
            *pw::keys::APP_NAME => "viewport",
            *pw::keys::NODE_NAME => "viewport-notification",
        },
    )
    .map_err(|e| anyhow::anyhow!("creating a pipewire stream: {e}"))?;

    // How far into `samples` the next buffer starts. Only the process
    // callback touches it, and PipeWire calls that from one thread.
    let position = std::cell::Cell::new(0usize);
    let audio = pcm.clone();
    let finishing = main_loop.downgrade();
    let finished = main_loop.downgrade();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let stride = audio.stride();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let Some(slice) = data.data() else {
                return;
            };

            // As many whole frames as fit in the buffer and remain in the
            // file, whichever runs out first.
            let start = position.get();
            let remaining = audio.samples.len().saturating_sub(start);
            let wanted = (slice.len() / stride) * audio.channels as usize;
            let taking = remaining.min(wanted);

            for (out, sample) in slice
                .chunks_exact_mut(std::mem::size_of::<f32>())
                .zip(&audio.samples[start..start + taking])
            {
                out.copy_from_slice(&sample.to_le_bytes());
            }
            position.set(start + taking);

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (taking / audio.channels as usize * stride) as u32;

            // The last buffer. Draining rather than quitting here: what has
            // been queued has not necessarily been heard yet, and tearing the
            // loop down now would cut the tail off every sound.
            if position.get() >= audio.samples.len() {
                if let Err(e) = stream.flush(true) {
                    tracing::debug!("draining a sound: {e}");
                    if let Some(main_loop) = finishing.upgrade() {
                        main_loop.quit();
                    }
                }
            }
        })
        .drained(move |_stream, ()| {
            if let Some(main_loop) = finished.upgrade() {
                main_loop.quit();
            }
        })
        .register()
        .map_err(|e| anyhow::anyhow!("listening to a pipewire stream: {e}"))?;

    let described = format(pcm)?;
    let mut params = [spa::pod::Pod::from_bytes(&described)
        .ok_or_else(|| anyhow::anyhow!("the format description is not a valid pod"))?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            // AUTOCONNECT so the session manager routes it to whatever the
            // default sink is, which is what a notification wants and what
            // makes this need no configuration. MAP_BUFFERS because the
            // process callback above writes into them with the CPU, and
            // RT_PROCESS because that callback is real-time safe: it copies
            // out of a Vec that was decoded before the stream existed.
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| anyhow::anyhow!("connecting a pipewire stream: {e}"))?;

    // A deadline inside the loop, so that a stream nothing ever routes ends
    // the thread it is on rather than parking it forever. The drain above
    // stays the way this normally finishes; the timer only fires if the drain
    // never does, and quitting from inside the loop is exactly what the drain
    // handler itself does.
    let timing_out = main_loop.downgrade();
    // Owned, because a source outlives this call: the callback has to carry
    // its own copy of the name.
    let unrouted = path.to_owned();
    let grace = main_loop.loop_().add_timer(move |_| {
        if let Some(main_loop) = timing_out.upgrade() {
            tracing::warn!(
                "nothing routed {} within {ROUTING_GRACE:?}; giving up on it",
                unrouted.display()
            );
            main_loop.quit();
        }
    });
    grace
        .update_timer(Some(ROUTING_GRACE), None)
        .into_result()
        .map_err(|e| anyhow::anyhow!("arming the sound timeout: {e}"))?;

    // Runs until the drain above quits it, or the timer does if no graph ever
    // takes an interest. The thread this is on exists for exactly this, and
    // everything opened here is dropped when it returns.
    main_loop.run();
    Ok(())
}

/// The audio format, as the pod `connect` wants.
fn format(pcm: &Pcm) -> anyhow::Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(pcm.rate);
    info.set_channels(pcm.channels);

    let values = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| anyhow::anyhow!("describing the audio format: {e}"))?
    .0
    .into_inner();
    Ok(values)
}

/// Read a sound file and decode it to interleaved `f32`.
///
/// Whole rather than streamed. These are alert sounds — the longest thing in
/// the freedesktop theme is under two seconds — so the file fits in memory
/// many times over, and holding it there is what lets the second notification
/// skip this entirely.
///
/// Before anything is read, the file is measured: the path arrives off the
/// bus, and the checks in [`MAX_FILE`] are what keep a hint from naming
/// something endless or enormous.
fn decode(path: &Path) -> anyhow::Result<Pcm> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::TrackType;
    use symphonia::core::io::MediaSourceStream;

    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    // A length check alone never fires on `/dev/zero`, which reports no size
    // at all, and a FIFO with no writer blocks forever — so the regular-file
    // check comes first, on the inode rather than the name.
    if !meta.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    if meta.len() > MAX_FILE {
        anyhow::bail!(
            "{} is {} bytes, past the {} a notification sound may be",
            path.display(),
            meta.len(),
            MAX_FILE
        );
    }
    let source = MediaSourceStream::new(Box::new(file), Default::default());

    // The extension, as a hint only. It is what distinguishes the `.oga` and
    // `.wav` a theme mixes freely, and probing still decides — a hint that is
    // wrong costs nothing.
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let mut reader = symphonia::default::get_probe()
        .probe(&hint, source, Default::default(), Default::default())
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("{} has no audio track", path.display()))?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
        anyhow::bail!("{} has no decodable audio track", path.display());
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &Default::default())
        .map_err(|e| anyhow::anyhow!("decoding {}: {e}", path.display()))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut rate = 0;
    let mut channels = 0;
    let mut frame = Vec::new();
    while let Some(packet) = reader
        .next_packet()
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?
    {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|e| anyhow::anyhow!("decoding {}: {e}", path.display()))?;
        rate = decoded.spec().rate();
        channels = decoded.spec().channels().count() as u32;
        decoded.copy_to_vec_interleaved(&mut frame);
        samples.append(&mut frame);
        // Defensively, past the probe: a file that decompresses large stops
        // here rather than growing until the allocation does.
        if samples.len() > MAX_SAMPLES {
            anyhow::bail!("{} decodes to more than the sample cap", path.display());
        }
    }

    if samples.is_empty() || rate == 0 || channels == 0 {
        anyhow::bail!("{} decoded to no audio", path.display());
    }
    Ok(Pcm {
        rate,
        channels,
        samples,
    })
}

/// Finding a named sound in the installed sound theme.
///
/// The XDG sound-theme lookup, which is what libcanberra would have done. The
/// shape of it: a name is looked for in a theme, a theme is a directory under
/// one of the data directories, each theme names the themes it inherits from,
/// and `freedesktop` is at the bottom of every chain because the specification
/// says every theme inherits it whether or not it says so.
mod theme {
    use std::path::{Path, PathBuf};

    /// Where a sound file may be, in the order the specification searches.
    ///
    /// `stereo` before the bare directory: the profile subdirectories are
    /// where every installed theme actually puts its files, and the flat
    /// layout is the older one. `mono` is not searched — a desktop with one
    /// speaker still plays the stereo file, and PipeWire downmixes it.
    const PROFILES: [&str; 2] = ["stereo", ""];

    /// Vorbis first, because that is what a theme ships; `.wav` because a
    /// hand-installed sound often is one.
    const EXTENSIONS: [&str; 3] = ["oga", "ogg", "wav"];

    /// The theme to look in when nothing says otherwise.
    ///
    /// The specification's own default, and the one every distribution
    /// installs. `XDG_SOUND_THEME` overrides it, which is what a desktop that
    /// lets its user choose a theme sets.
    const DEFAULT: &str = "freedesktop";

    /// The file a name resolves to, or nothing.
    pub fn find(name: &str) -> Option<PathBuf> {
        // A name with a path separator in it is not a name. Refusing rather
        // than joining it: a sender that puts "../../etc/passwd" in a
        // `sound-name` hint must not reach outside the theme directories, and
        // a legitimate name never contains one.
        if name.contains('/') || name.contains('\\') || name.is_empty() {
            return None;
        }

        let base = base_directories();
        let mut seen = Vec::new();
        let mut queue = vec![current_theme()];

        // Breadth-first through the inheritance chain, which is a graph and
        // not a tree — two themes may inherit the same third, and one that
        // inherits itself would otherwise be an infinite loop.
        while let Some(theme) = queue.pop() {
            if seen.contains(&theme) {
                continue;
            }
            for directory in &base {
                if let Some(found) = look_in(&directory.join(&theme), name) {
                    return Some(found);
                }
            }
            queue.extend(inherits(&base, &theme));
            seen.push(theme);
        }

        // And outside any theme. The specification's last resort, and where a
        // locally dropped sound with no theme around it lives.
        base.iter().find_map(|directory| look_in(directory, name))
    }

    /// One theme directory, across its profiles and extensions.
    fn look_in(theme: &Path, name: &str) -> Option<PathBuf> {
        for profile in PROFILES {
            let directory = if profile.is_empty() {
                theme.to_path_buf()
            } else {
                theme.join(profile)
            };
            for extension in EXTENSIONS {
                let candidate = directory.join(format!("{name}.{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// `$XDG_DATA_HOME/sounds`, then every `$XDG_DATA_DIRS` entry's, in the
    /// order the specification gives them: the user's own first, so a sound
    /// dropped in a home directory wins over the system's.
    fn base_directories() -> Vec<PathBuf> {
        let mut directories = Vec::new();

        if let Some(home) = data_home() {
            directories.push(home.join("sounds"));
        }
        // ~/.sounds, which the specification lists and nothing writes any
        // more. Cheap to look at and the reason someone's decade-old sound
        // still plays.
        if let Some(home) = std::env::var_os("HOME") {
            directories.push(PathBuf::from(home).join(".sounds"));
        }

        let dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for entry in std::env::split_paths(&dirs) {
            directories.push(entry.join("sounds"));
        }
        directories
    }

    fn data_home() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            if !dir.is_empty() {
                return Some(PathBuf::from(dir));
            }
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
    }

    fn current_theme() -> String {
        std::env::var("XDG_SOUND_THEME")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT.to_owned())
    }

    /// The themes a theme inherits, from its `index.theme`.
    ///
    /// `freedesktop` is appended to every answer rather than only to a theme
    /// that declares it, because the specification makes it the implicit
    /// parent of everything — a theme whose `index.theme` names no parent
    /// still falls back to it, and without this a partial theme would silence
    /// every sound it does not itself carry.
    fn inherits(base: &[PathBuf], theme: &str) -> Vec<String> {
        let mut parents = Vec::new();
        for directory in base {
            let Ok(text) = std::fs::read_to_string(directory.join(theme).join("index.theme"))
            else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                let Some(value) = line.strip_prefix("Inherits") else {
                    continue;
                };
                let Some(value) = value.trim_start().strip_prefix('=') else {
                    continue;
                };
                parents.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        if theme != super::theme::DEFAULT {
            parents.push(DEFAULT.to_owned());
        }
        parents
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_name_with_a_separator_is_refused() {
            // A `sound-name` hint comes from any program on the bus. Joining
            // one onto a theme directory unchecked is how it reaches a file
            // outside every theme.
            assert_eq!(find("../../../etc/passwd"), None);
            assert_eq!(find("subdir/bell"), None);
            assert_eq!(find(""), None);
        }

        #[test]
        fn the_profile_directory_is_searched_before_the_flat_one() {
            // Every installed theme uses `stereo/`; the flat layout is older
            // and, where both exist, the profile is the one meant.
            assert_eq!(PROFILES[0], "stereo");
        }

        #[test]
        fn freedesktop_is_the_parent_of_every_other_theme() {
            // A theme that carries three sounds and no `Inherits` line still
            // has to fall through to the theme that carries the rest.
            let parents = inherits(&[], "some-partial-theme");
            assert_eq!(parents, vec![DEFAULT.to_owned()]);
        }

        #[test]
        fn the_default_theme_does_not_inherit_itself() {
            // It is its own last resort; listing it again would be a cycle
            // for `find` to notice and skip, which is work for nothing.
            assert!(inherits(&[], DEFAULT).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_file_is_played_as_a_path() {
        assert_eq!(
            Sound::from_config(Some("/sounds/bark.ogg"), None),
            Some(Sound::File("/sounds/bark.ogg".to_owned()))
        );
    }

    #[test]
    fn a_configured_name_is_looked_up_in_the_theme() {
        assert_eq!(
            Sound::from_config(None, Some("message-new-instant")),
            Some(Sound::Name("message-new-instant".to_owned()))
        );
    }

    #[test]
    fn a_file_wins_over_a_name() {
        // As among the hints: the path is unambiguous, and the name may
        // resolve to nothing at all in the installed theme.
        assert_eq!(
            Sound::from_config(Some("/sounds/bark.ogg"), Some("bell")),
            Some(Sound::File("/sounds/bark.ogg".to_owned()))
        );
    }

    #[test]
    fn nothing_configured_is_no_sound() {
        assert_eq!(Sound::from_config(None, None), None);
    }

    #[test]
    fn an_empty_value_takes_the_sound_away() {
        // Blanking the value is how a configuration says "silent" without
        // having to delete the key, which is the reading `wallpaper` gives an
        // empty string too.
        assert_eq!(Sound::from_config(Some(""), None), None);
        assert_eq!(Sound::from_config(Some(""), Some("")), None);
        // And an empty file still falls through to a name that is set, rather
        // than swallowing it.
        assert_eq!(
            Sound::from_config(Some(""), Some("bell")),
            Some(Sound::Name("bell".to_owned()))
        );
    }

    #[test]
    fn a_path_that_is_not_there_resolves_to_nothing() {
        // Rather than being handed to the decoder to fail on. A configuration
        // pointing at a sound that was uninstalled is silent, not an error
        // per notification.
        assert_eq!(
            Sound::File("/nonexistent/nothing-here.ogg".to_owned()).resolve(),
            None
        );
    }

    #[test]
    fn a_file_resolves_to_itself() {
        let path = std::env::current_exe().expect("the test binary has a path");
        let sound = Sound::File(path.to_string_lossy().into_owned());
        assert_eq!(sound.resolve(), Some(path));
    }

    /// Decode and play a real file, on a machine that has a sound server.
    ///
    /// Ignored by default and not because it is slow: it needs PipeWire, an
    /// output device and a sound file to point at, none of which a CI runner
    /// has — and it makes a noise, which a test suite someone is running
    /// while listening to something else should not do unasked.
    ///
    ///     VIEWPORT_SOUND=/usr/share/sounds/freedesktop/stereo/bell.oga \
    ///         cargo test -p viewport --bin viewport -- --ignored play_
    #[test]
    #[ignore]
    fn playing_a_real_file_makes_a_sound() {
        let Ok(path) = std::env::var("VIEWPORT_SOUND") else {
            panic!("set VIEWPORT_SOUND to a sound file to run this");
        };
        let decoded = decode(Path::new(&path)).expect("the file should decode");
        assert!(decoded.rate >= 8_000, "an implausible rate for a sound");
        assert!(!decoded.samples.is_empty(), "decoded to nothing");

        Player::new().expect("this machine should have pipewire");

        // Synchronously, rather than through `play`, which spawns and returns
        // — and would pass whether or not a single sample was ever consumed.
        // `stream` returns when the drain fires, and the drain fires only
        // after the graph has taken every buffer, so returning at all is the
        // assertion. A stream nothing routed is ended by the routing timeout
        // instead, and the elapsed-time assertion below is what reports it.
        let started = std::time::Instant::now();
        stream(Path::new(&path), &Arc::new(decoded)).expect("the sound should play");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the drain took implausibly long"
        );
    }

    #[test]
    fn interleaved_frames_are_measured_by_the_channel_count() {
        let pcm = Pcm {
            rate: 48_000,
            channels: 2,
            samples: vec![0.0; 8],
        };
        assert_eq!(pcm.stride(), 8, "two f32 samples to a frame");
    }
}
