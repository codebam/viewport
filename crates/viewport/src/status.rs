// SPDX-License-Identifier: GPL-3.0-or-later
//
// System statistics for the shell's status bar. Ports src/status.c.
//
// This is the one part of a status bar that cannot live in the shell. The page
// is loaded from file:// or http://, and neither origin can read /proc — so
// the numbers have to be sampled here and sent over, even though everything
// about how they are *displayed* is the shell's business.
//
// The parsing is separated from the reading so it can be tested against real
// /proc text rather than against whatever this machine happens to report.

use std::time::{Duration, Instant};

/// One sample, as the shell is told it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sample {
    /// Percentage, or `None` before there are two samples to compare.
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub load: [f64; 3],
    /// Bytes per second since the last sample.
    pub net_rx: f64,
    pub net_tx: f64,
    /// Free and total bytes on the root filesystem, as the default bar shows.
    pub disk_free: f64,
    pub disk_total: f64,
    /// Free and total bytes on each extra mount a bar widget asked for.
    pub mounts: Vec<MountUsage>,
    /// The default audio sink's volume in `0.0..=1.0`, or `None` when nothing
    /// said, e.g. no `wpctl` or no sink.
    pub volume: Option<f64>,
    /// Whether the default audio sink is muted. `None` alongside `volume`
    /// being `None`; a muted sink still reports a volume, so this is only
    /// meaningful when `volume` is `Some`.
    pub muted: Option<bool>,
    /// The default audio source's (microphone) volume in `0.0..=1.0`, or
    /// `None` when nothing said, e.g. no `wpctl` or no source.
    pub mic_volume: Option<f64>,
    /// Whether the default audio source is muted; meaningful only when
    /// `mic_volume` is `Some`.
    pub mic_muted: Option<bool>,
    /// Panel brightness, populated after a built-in brightness change.
    pub brightness: Option<f64>,
}

/// Free and total bytes on one mount a bar widget asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct MountUsage {
    pub path: String,
    pub free: f64,
    pub total: f64,
}

/// A sample reduced to exactly what the shipped bar draws from it.
///
/// The sampler reads `/proc` and the worker runs `statvfs`/`wpctl` every two
/// seconds whether or not anything moved. Publishing that sample wakes every
/// shell engine for a JS evaluation, and an engine woken on an idle desktop is
/// a composited frame — so the periodic tick compares this against the last
/// sample it published and says nothing when the strings and percentages are
/// the same. Every rule below mirrors `data/shell/bar.js`; where the shell
/// rounds, this rounds, and where it hides a value this records that too.
#[derive(Debug, Clone, PartialEq)]
struct Displayed {
    cpu: Option<i64>,
    memory: Option<i64>,
    /// `toFixed(2)` of the load module, stored as the hundredths it writes.
    load: i64,
    /// `formatBytes` of the root filesystem's free space, or `None` when the
    /// disk module draws nothing.
    disk_free: Option<DisplayedBytes>,
    net_rx: DisplayedBytes,
    net_tx: DisplayedBytes,
    /// One entry per configured mount, in order, as the disk widget would find
    /// them.
    mounts: Vec<DisplayedMount>,
    /// Rounded percent and mute state; `None` where the widget draws nothing.
    volume: Option<(i64, bool)>,
    mic: Option<(i64, bool)>,
    // Brightness is deliberately absent: the only shipped drawing of it is the
    // OSD, and an OSD sample is always published by the caller rather than
    // compared here.
}

/// What `formatBytes` writes, as a comparable number rather than a string.
///
/// The shell uses `toFixed(1)` below ten and `Math.round` at or above it, so
/// those are the two cases kept apart: a value that draws `9.9K` and one that
/// draws `10K` must not compare equal merely because both have a `10` in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayedBytes {
    /// `'0B'`, which is also what a missing or non-finite figure writes.
    Zero,
    /// `n.toFixed(1)` followed by the unit, stored as the tenths it writes.
    Tenths { unit: u8, tenths: i64 },
    /// `Math.round(n)` followed by the unit, stored as that integer.
    Rounded { unit: u8, rounded: i64 },
}

/// One configured mount as the disk widget writes it: the path it matched on
/// and what it draws, or `None` when the widget stays hidden.
#[derive(Debug, Clone, PartialEq)]
struct DisplayedMount {
    path: String,
    text: Option<DisplayedBytes>,
}

impl Displayed {
    fn of(sample: &Sample) -> Self {
        Self {
            cpu: display_percent(sample.cpu),
            memory: display_percent(sample.memory),
            // `toFixed(2)`: nearest hundredth, stored as the integer the
            // module's text is made of.
            load: (sample.load[0] * 100.0).round() as i64,
            // `s.disk_free ? ... : ''` — zero and NaN are the hidden cases,
            // while a negative figure is truthy and formats as "0B".
            disk_free: if sample.disk_free == 0.0 || sample.disk_free.is_nan() {
                None
            } else {
                Some(display_bytes(sample.disk_free))
            },
            net_rx: display_bytes(sample.net_rx),
            net_tx: display_bytes(sample.net_tx),
            mounts: sample.mounts.iter().map(displayed_mount).collect(),
            volume: display_audio(sample.volume, sample.muted),
            mic: display_audio(sample.mic_volume, sample.mic_muted),
        }
    }
}

fn displayed_mount(mount: &MountUsage) -> DisplayedMount {
    DisplayedMount {
        path: mount.path.clone(),
        // The widget writes nothing at all when the mount could not be
        // measured; `total > 0` is the shell's own test for that.
        text: (mount.total > 0.0).then(|| display_bytes(mount.free)),
    }
}

/// `formatBytes` from `data/shell/bar.js`, reduced to the number the widget
/// actually writes plus its unit.
///
/// The numbers, rather than the formatted strings, are compared because Rust
/// and JavaScript round exact halves differently when formatting (`toFixed`
/// ties upwards, Rust's formatter ties to even); reducing to the written
/// integer avoids a display difference hiding behind the two formatters.
fn display_bytes(n: f64) -> DisplayedBytes {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if !n.is_finite() || n <= 0.0 {
        return DisplayedBytes::Zero;
    }
    let mut n = n;
    let mut unit = 0u8;
    while n >= 1024.0 && unit < (UNITS.len() - 1) as u8 {
        n /= 1024.0;
        unit += 1;
    }
    // `toFixed(1)` under ten, `Math.round` at and above it.
    if n < 10.0 {
        DisplayedBytes::Tenths {
            unit,
            tenths: (n * 10.0).round() as i64,
        }
    } else {
        DisplayedBytes::Rounded {
            unit,
            rounded: n.round() as i64,
        }
    }
}

/// The CPU and memory modules' `Math.round`, with the `-1.0` sentinel and
/// anything below it drawing nothing.
fn display_percent(value: Option<f64>) -> Option<i64> {
    let value = value?;
    if value < 0.0 {
        return None;
    }
    Some(value.round() as i64)
}

/// The volume and mic widgets: `Math.round(volume * 100)` plus the mute glyph,
/// hidden until the compositor can report a volume.
fn display_audio(volume: Option<f64>, muted: Option<bool>) -> Option<(i64, bool)> {
    let volume = volume.filter(|volume| *volume >= 0.0)?;
    Some(((volume * 100.0).round() as i64, muted.unwrap_or(false)))
}

/// Totals from `/proc/stat`, which are cumulative and only useful as a delta.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
pub fn parse_cpu(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    // user, nice, system, idle, iowait, irq, softirq, steal.
    if fields.len() < 8 {
        return None;
    }
    // iowait counts as idle: the processor was not doing work.
    let idle = fields[3] + fields[4];
    Some(CpuTimes {
        total: fields[..8].iter().sum(),
        idle,
    })
}

/// The fraction of time spent working between two samples.
pub fn cpu_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    if previous.total == 0 || current.total <= previous.total {
        return None;
    }
    let total = (current.total - previous.total) as f64;
    let idle = current.idle.saturating_sub(previous.idle) as f64;
    Some(100.0 * (total - idle) / total)
}

/// Used memory as a percentage, matching what `free` calls "used".
pub fn parse_memory(text: &str) -> Option<f64> {
    let mut total = 0u64;
    let mut available = 0u64;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match key {
            "MemTotal" => total = value,
            // Not MemFree: that excludes reclaimable cache, and a machine with
            // a warm page cache would read as almost full.
            "MemAvailable" => available = value,
            _ => {}
        }
    }
    if total == 0 {
        return None;
    }
    Some(100.0 * (total - available.min(total)) as f64 / total as f64)
}

/// Cumulative receive and transmit bytes across every real interface.
pub fn parse_network(text: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    // Two header lines.
    for line in text.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        // Loopback traffic is not "network activity" in any useful sense.
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = counters
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        if fields.len() < 9 {
            continue;
        }
        rx += fields[0];
        tx += fields[8];
    }
    (rx, tx)
}

/// A rate, or zero when the counters cannot be compared.
///
/// Both counters are checked, not just the one being asked about. They are
/// sums over the interfaces that exist right now, so an interface going away —
/// a VPN dropping, a dock unplugged — makes the total go backwards. Treating
/// that as a delta gave one sample of some exabytes per second
/// (`src/status.c:141`).
pub fn rate(previous: u64, current: u64, seconds: f64) -> f64 {
    if seconds <= 0.0 || current < previous {
        return 0.0;
    }
    (current - previous) as f64 / seconds
}

/// The half of a sample that is not a `/proc` read: filesystems, which a dead
/// NFS mount answers for never, and `wpctl`, which is two processes to fork,
/// exec and wait for.
///
/// Split out because the compositor must not be the thread that waits for any
/// of it. See [`Status::start`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Slow {
    /// Free and total bytes on the root filesystem.
    pub disk_free: f64,
    pub disk_total: f64,
    pub mounts: Vec<MountUsage>,
    pub volume: Option<f64>,
    pub muted: Option<bool>,
    pub mic_volume: Option<f64>,
    pub mic_muted: Option<bool>,
    pub brightness: Option<f64>,
    pub osd: Option<viewport_ipc::event::StatusOsd>,
    /// Whether this answers a sampling job rather than a change asked for by
    /// [`Status::set_audio`]. Only a sampling job's answer frees the slot the
    /// next tick asks in; a volume scroll must not be mistaken for one, or a
    /// tick queues a second sample while the first is still being taken.
    pub sampled: bool,
}

/// What the worker is asked for, which is whatever the bar's widgets want.
#[derive(Debug, Clone, Default)]
struct Job {
    /// `wpctl` invocations to run before measuring, for a job that changes
    /// something rather than only reading it. Run first and waited for, so the
    /// measurement that follows describes the sink as the change left it —
    /// see [`Status::set_audio`].
    set: Vec<Vec<String>>,
    mounts: Vec<String>,
    want_volume: bool,
    want_mic: bool,
    brightness: Option<i32>,
    osd: Option<viewport_ipc::event::StatusOsd>,
}

/// Samples the machine, keeping what it needs to turn counters into rates.
pub struct Status {
    cpu: CpuTimes,
    rx: u64,
    tx: u64,
    at: Option<Instant>,
    /// Extra mounts to report, from the bar's disk widgets.
    mounts: Vec<String>,
    /// Whether a volume widget asked for the default sink's volume.
    want_volume: bool,
    /// Whether a mic widget asked for the default source's volume.
    want_mic: bool,
    /// The worker's mailbox, once there is a worker. Absent in a test and in
    /// anything that samples before the loop exists, where the slow half is
    /// simply read on the spot as it always was.
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// Whether the worker is already busy with a job. One at a time: a sampler
    /// that queues faster than `wpctl` runs would never catch up.
    asked: bool,
    /// The last answer the worker gave, which is what a sample reports. At most
    /// one tick old, and a bar showing a two-second-old volume is a bar; a
    /// compositor waiting two seconds for one is a freeze.
    slow: Slow,
    /// Whether the shell's bar draws anything this sampler produces. False
    /// lets the periodic tick return without reading `/proc` or queueing a
    /// `statvfs`/`wpctl` job at all; `configure` sets it from the bar
    /// configuration. True by default because an unconfigured sampler has
    /// always sampled — only an explicit no-status bar turns it off.
    wanted: bool,
    /// What the last published sample looked like on screen, so the next tick
    /// can tell whether there is anything to say.
    published: Option<Displayed>,
    /// When that was, for the heartbeat that keeps a freshly started or
    /// restarted shell from having to wait for a change to get state.
    published_at: Option<Instant>,
    /// Set when a sample must go out even if the displayed values have not
    /// moved: a shell has connected or restarted, or the bar configuration
    /// changed under it.
    force_publish: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            cpu: CpuTimes::default(),
            rx: 0,
            tx: 0,
            at: None,
            mounts: Vec::new(),
            want_volume: false,
            want_mic: false,
            jobs: None,
            asked: false,
            slow: Slow::default(),
            // An unconfigured sampler is the pre-existing behaviour: sample.
            wanted: true,
            published: None,
            published_at: None,
            force_publish: false,
        }
    }
}

impl Status {
    /// Which mounts, and which audio nodes' volumes, to sample on the next
    /// ticks, and whether the bar draws any of it.
    ///
    /// Called from config application; `Status::default()` wants no extra
    /// mounts and no `wpctl`, which is a bar with no such widgets and thus no
    /// reason to stat or spawn anything for them. `wanted` is the wider
    /// question: a `bar_items` override with no status module and no
    /// status-reading widget has nothing this sampler can change, and the
    /// periodic tick returns without sampling at all.
    pub fn configure(
        &mut self,
        mounts: Vec<String>,
        want_volume: bool,
        want_mic: bool,
        wanted: bool,
    ) {
        self.mounts = mounts;
        self.want_volume = want_volume;
        self.want_mic = want_mic;
        self.wanted = wanted;
        // A new widget set has to be told to the shell even when every value
        // in it happens to match the last sample, and a shell that has just
        // restarted has no baseline at all.
        self.force_publish = true;
    }

    /// Whether the periodic tick has any reason to sample at all.
    pub fn wanted(&self) -> bool {
        self.wanted
    }

    /// Make the next periodic sample publish regardless of what changed.
    ///
    /// Used when a shell connects or restarts: its page starts with an empty
    /// status object, and the change-driven tick could otherwise stay quiet
    /// for the whole heartbeat.
    pub fn force_publish(&mut self) {
        self.force_publish = true;
    }

    /// Remember a sample that has just been sent, so a later change-driven
    /// tick compares against what the shell actually has rather than against
    /// the previous raw reading.
    pub fn record_published(&mut self, sample: &Sample) {
        self.published = Some(Displayed::of(sample));
        self.published_at = Some(Instant::now());
        self.force_publish = false;
    }

    /// Decide whether a freshly taken sample is worth publishing on the
    /// periodic tick, recording it when it is.
    ///
    /// `now` is passed in so the heartbeat can be tested without sleeping.
    pub fn should_publish_periodic(
        &mut self,
        sample: &Sample,
        heartbeat: Duration,
        now: Instant,
    ) -> bool {
        let displayed = Displayed::of(sample);
        let changed = self.published.as_ref() != Some(&displayed);
        let overdue = self
            .published_at
            .is_none_or(|at| now.saturating_duration_since(at) >= heartbeat);
        if !(self.force_publish || changed || overdue) {
            return false;
        }
        self.published = Some(displayed);
        self.published_at = Some(now);
        self.force_publish = false;
        true
    }

    /// Start the thread that does the waiting, and deliver its answers through
    /// `sink`.
    ///
    /// Everything here used to be sampled on the compositor's own loop, on a
    /// two-second timer: two `wpctl get-volume` processes forked, exec'd and
    /// waited for, and a `statvfs` per configured mount. A bar with a volume
    /// and a mic widget therefore stalled the compositor on a fixed cadence,
    /// and one dead NFS mount stalled it for good — `statvfs` on an
    /// unresponsive server does not return.
    ///
    /// So it happens on a thread, and the answer arrives through a calloop
    /// channel like the notification service's does. The compositor never
    /// waits: a tick reports the last answer and asks for the next.
    ///
    /// A failure to spawn is not fatal — the sampler falls back to reading the
    /// slow half in line, which is what it did before there was a thread.
    pub fn start(
        &mut self,
        sink: smithay::reexports::calloop::channel::Sender<Slow>,
    ) -> std::io::Result<()> {
        // So the first tick has a disk figure rather than a zero. Nothing is
        // configured yet at startup, so this is one `statvfs` on the root
        // filesystem and no subprocess at all.
        self.slow = measure(&Job::default());

        let (sender, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("viewport-status".to_owned())
            .spawn(move || {
                for job in jobs {
                    // A closed channel is the compositor going away.
                    if sink.send(measure(&job)).is_err() {
                        return;
                    }
                }
            })?;
        self.jobs = Some(sender);
        Ok(())
    }

    /// Take the worker's answer, and say whether the shell should hear about it
    /// before the next tick.
    ///
    /// Audio changes are reported before the next tick so their widgets and OSD
    /// update immediately. Brightness feedback is returned separately without
    /// replacing unrelated cached status fields. Disk figures drift by a block
    /// on any busy machine and are not worth a message of their own.
    pub fn absorb(&mut self, slow: Slow) -> (bool, Option<viewport_ipc::event::StatusOsd>) {
        if slow.sampled {
            self.asked = false;
        }
        // A brightness job samples only the backlight. Replacing the complete
        // snapshot with that answer would briefly erase audio and mount widgets.
        if slow.osd == Some(viewport_ipc::event::StatusOsd::Brightness) {
            self.slow.brightness = slow.brightness;
            self.slow.osd = slow.osd;
            return (false, slow.osd);
        }
        let audio_changed = (slow.volume, slow.muted, slow.mic_volume, slow.mic_muted)
            != (
                self.slow.volume,
                self.slow.muted,
                self.slow.mic_volume,
                self.slow.mic_muted,
            );
        let osd = slow.osd;
        self.slow = slow;
        (audio_changed, osd)
    }

    /// Change an audio node's volume or mute state, and sample it afterwards.
    ///
    /// The ordering is the whole point, and it is why this cannot be
    /// `input::spawn`: the shell used to spawn `wpctl` and ask for a refresh in
    /// the next message, which samples the sink before the child that changes
    /// it has run — a scroll that worked, drawing the number that was already
    /// there. So the change is waited for and the measurement follows it.
    ///
    /// The waiting is the worker's, not the compositor's. Two `wpctl` runs on
    /// the event loop are two forks and two execs a scroll wheel can ask for
    /// faster than they finish; the jobs are serial, so the sink ends where the
    /// last scroll put it either way.
    ///
    /// Returns whether the caller must sample on the spot, which is true only
    /// where there is no worker to answer: a test, or a message handled before
    /// the loop exists.
    pub fn set_audio(&mut self, node: &str, delta: Option<i32>, mute: bool) -> bool {
        let set = audio_args(node, delta, mute);
        if set.is_empty() {
            return false;
        }

        // The node that was just changed is read back whether or not a widget
        // asked for it standingly: something asked about it by changing it, and
        // the answer is what tells the shell the scroll landed.
        let sink = node == SINK;
        let job = Job {
            set,
            mounts: self.mounts.clone(),
            want_volume: self.want_volume || sink,
            want_mic: self.want_mic || !sink,
            osd: Some(if sink {
                viewport_ipc::event::StatusOsd::Volume
            } else {
                viewport_ipc::event::StatusOsd::Microphone
            }),
            ..Job::default()
        };
        // Queued whatever else is in flight, unlike a sampling job: a scroll
        // that is dropped is a volume that does not move.
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return false,
                // A worker that has gone hands the job back.
                Err(std::sync::mpsc::SendError(job)) => job,
            },
            None => job,
        };

        // No worker: do it here as it was always done, and let the caller
        // report the result.
        self.absorb(measure(&job));
        true
    }

    /// Change panel brightness and read back the result on the status worker.
    pub fn set_brightness(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let job = Job {
            brightness: Some(delta),
            osd: Some(viewport_ipc::event::StatusOsd::Brightness),
            ..Job::default()
        };
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return false,
                Err(std::sync::mpsc::SendError(job)) => job,
            },
            None => job,
        };
        self.absorb(measure(&job));
        true
    }

    /// Read everything once.
    ///
    /// A file that cannot be read leaves its own figure absent rather than
    /// failing the sample: a container without /proc/net/dev should still
    /// report a CPU percentage.
    pub fn sample(&mut self) -> Sample {
        let now = Instant::now();
        let mut sample = Sample::default();

        if let Some(times) = std::fs::read_to_string("/proc/stat")
            .ok()
            .as_deref()
            .and_then(parse_cpu)
        {
            sample.cpu = cpu_percent(self.cpu, times);
            self.cpu = times;
        }

        sample.memory = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(parse_memory);

        if let Ok(text) = std::fs::read_to_string("/proc/net/dev") {
            let (rx, tx) = parse_network(&text);
            let seconds = self
                .at
                .map(|at| now.duration_since(at).as_secs_f64())
                .unwrap_or(0.0);
            sample.net_rx = rate(self.rx, rx, seconds);
            sample.net_tx = rate(self.tx, tx, seconds);
            self.rx = rx;
            self.tx = tx;
        }

        sample.load = load_average();

        // The half that can wait: the last answer the worker gave, and a fresh
        // job asked for. Read in line only where there is no worker — a test,
        // or a sample taken before the loop exists.
        let job = Job {
            set: Vec::new(),
            mounts: self.mounts.clone(),
            want_volume: self.want_volume,
            want_mic: self.want_mic,
            ..Job::default()
        };
        match &self.jobs {
            Some(jobs) => {
                if !self.asked {
                    // One in flight at a time; a send that fails is a worker
                    // that has gone, and the last answer stands.
                    self.asked = jobs.send(job).is_ok();
                }
            }
            None => {
                // A just-completed on-demand read stays available when this
                // unthreaded fallback samples unrelated fields. Production has
                // a worker; tests and very early startup do not.
                let previous = self.slow.clone();
                self.slow = measure(&job);
                if !job.want_volume {
                    self.slow.volume = previous.volume;
                    self.slow.muted = previous.muted;
                }
                if !job.want_mic {
                    self.slow.mic_volume = previous.mic_volume;
                    self.slow.mic_muted = previous.mic_muted;
                }
                self.slow.brightness = previous.brightness;
            }
        }

        sample.disk_free = self.slow.disk_free;
        sample.disk_total = self.slow.disk_total;
        // The extra mounts a bar widget asked about, one entry per configured
        // path. A widget that names `/` reports the root mount as its own
        // entry, which is that widget's business — the default disk module
        // next to it is a separate element.
        sample.mounts = self.slow.mounts.clone();
        sample.volume = self.slow.volume;
        sample.muted = self.slow.muted;
        sample.mic_volume = self.slow.mic_volume;
        sample.mic_muted = self.slow.mic_muted;
        sample.brightness = self.slow.brightness;

        self.at = Some(now);
        sample
    }
}

/// Everything that has to be waited for, in one pass. On the worker thread,
/// except where there is no worker.
fn measure(job: &Job) -> Slow {
    // Before anything is read: a job that changes the sink is measured as the
    // change left it, which is the reason it waits for `wpctl` at all.
    for args in &job.set {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        wpctl(&args);
    }
    let brightness = job.brightness.and_then(change_brightness);

    let (disk_free, disk_total) = disk_usage("/");
    let mut slow = Slow {
        disk_free,
        disk_total,
        sampled: job.set.is_empty() && job.brightness.is_none(),
        brightness,
        osd: job.osd,
        ..Slow::default()
    };

    for path in &job.mounts {
        let (free, total) = disk_usage(path);
        slow.mounts.push(MountUsage {
            path: path.clone(),
            free,
            total,
        });
    }

    if job.want_volume {
        let (volume, muted) = audio_state(SINK);
        slow.volume = volume;
        slow.muted = muted;
    }

    if job.want_mic {
        let (volume, muted) = audio_state(SOURCE);
        slow.mic_volume = volume;
        slow.mic_muted = muted;
    }

    slow
}

fn change_brightness(delta: i32) -> Option<f64> {
    let step = relative_percent(delta);
    let output = std::process::Command::new("brightnessctl")
        .args(["-m", "set", &step])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::warn!("brightnessctl set {step}: {}", output.status);
        return None;
    }
    parse_brightness(&String::from_utf8_lossy(&output.stdout))
}

fn parse_brightness(text: &str) -> Option<f64> {
    text.split(|c: char| c == ',' || c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|part| part.strip_suffix('%'))
        .find_map(|part| part.parse::<f64>().ok())
        .map(|percent| (percent / 100.0).clamp(0.0, 1.0))
}

/// The one, five and fifteen minute load averages.
fn load_average() -> [f64; 3] {
    let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
        return [0.0; 3];
    };
    let mut values = text.split_whitespace();
    let mut out = [0.0; 3];
    for slot in out.iter_mut() {
        *slot = values.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    }
    out
}

/// Free and total bytes on the filesystem holding `path`.
fn disk_usage(path: &str) -> (f64, f64) {
    use smithay::reexports::rustix::fs::statvfs;

    let Ok(stat) = statvfs(path) else {
        return (0.0, 0.0);
    };
    // f_bavail, not f_bfree: the blocks a normal process may actually use,
    // which is what "free" means to someone reading a bar.
    let frsize = stat.f_frsize as f64;
    (stat.f_bavail as f64 * frsize, stat.f_blocks as f64 * frsize)
}

/// The `wpctl` invocations one `status.volume` asks for: a relative step, a
/// mute toggle, or both, in that order.
///
/// A delta of zero and no mute is a message that asks for nothing, and asking
/// `wpctl` for a zero step is still two processes; it comes back empty and
/// nothing is queued.
fn audio_args(node: &str, delta: Option<i32>, mute: bool) -> Vec<Vec<String>> {
    let mut set = Vec::new();
    if let Some(delta) = delta.filter(|delta| *delta != 0) {
        // `wpctl` spells a relative change with the sign after the unit, and
        // takes no negative number: 5%+ up, 5%- down.
        let step = relative_percent(delta);
        set.push(vec!["set-volume".to_owned(), node.to_owned(), step]);
    }
    if mute {
        set.push(vec![
            "set-mute".to_owned(),
            node.to_owned(),
            "toggle".to_owned(),
        ]);
    }
    set
}

fn relative_percent(delta: i32) -> String {
    if delta > 0 {
        format!("{delta}%+")
    } else {
        format!("{}%-", delta.unsigned_abs())
    }
}

/// The names `wpctl` knows the default sink and source by. A page names a
/// target as `sink` or `source` and gets one of these; it cannot name a node
/// itself, `wpctl` taking an id wherever it does not recognise a name.
pub const SINK: &str = "@DEFAULT_AUDIO_SINK@";
pub const SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

/// Run `wpctl` and wait for it, on the worker thread, for a change the bar
/// reports. See [`Status::set_audio`], which is the only thing that asks.
///
/// Returns whether it ran and said it succeeded. A machine with no `wpctl`,
/// no session bus or no sink says so by failing here, and nothing about that
/// should take the bar or the compositor down: the measurement that follows
/// reports whatever is true.
fn wpctl(args: &[&str]) -> bool {
    match std::process::Command::new("wpctl").args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!("wpctl {}: {status}", args.join(" "));
            false
        }
        Err(e) => {
            tracing::warn!("wpctl {}: {e}", args.join(" "));
            false
        }
    }
}

/// The default audio node's volume and mute state, from the session's
/// PipeWire.
///
/// `node` is `@DEFAULT_AUDIO_SINK@` for the speakers a `volume` widget reads,
/// or `@DEFAULT_AUDIO_SOURCE@` for the microphone a `mic` widget reads. The
/// page cannot ask for either, any more than it can read /proc, so the
/// compositor does, through `wpctl` — the WirePlumber command line, which is
/// what a person would use for the same question. One call answers both
/// halves of one node, so a sampled tick pays for a single subprocess per
/// widget kind and nothing else.
///
/// Every failure resolves to `None` rather than an error: no `wpctl`, no
/// session bus, no sink or source — all ordinary on a machine the widget is
/// not wanted on — and none of them should take the bar down.
fn audio_state(node: &str) -> (Option<f64>, Option<bool>) {
    let Ok(output) = std::process::Command::new("wpctl")
        .arg("get-volume")
        .arg(node)
        .output()
    else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }
    parse_sink(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `wpctl get-volume <node>` output.
///
/// The interesting lines are `Volume: 0.45` and `Muted: yes`. `Volume` may be
/// `0.00` for a muted node, which is still a volume; the two are independent.
/// Modern `wpctl` folds the mute state into the volume line itself as a `[...]`
/// marker (`Volume: 0.45 [MUTED]`) and prints no `Muted:` line at all, so both
/// forms are read.
fn parse_sink(text: &str) -> (Option<f64>, Option<bool>) {
    let mut volume = None;
    let mut muted = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Volume:") {
            // A bracketed marker is always the mute state, which on modern
            // wpctl rides on the volume line instead of a `Muted:` line.
            if rest.contains("[MUTED]") || rest.contains('[') {
                muted = Some(true);
            }
            // Take the leading number, discarding any trailing annotation so
            // `0.60 [MUTED]` still reads as a volume.
            volume = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok());
        } else if let Some(rest) = line.strip_prefix("Muted:") {
            muted = match rest.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
    }
    (volume, muted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT: &str = "\
cpu  100 20 30 500 10 0 5 0 0 0
cpu0 50 10 15 250 5 0 2 0 0 0
intr 12345
";

    #[test]
    fn the_aggregate_cpu_line_is_the_one_that_counts() {
        let times = parse_cpu(PROC_STAT).expect("should parse");
        // 100+20+30+500+10+0+5+0
        assert_eq!(times.total, 665);
        // idle + iowait, because neither was doing work.
        assert_eq!(times.idle, 510);
    }

    #[test]
    fn the_first_sample_has_nothing_to_compare_against() {
        // A percentage needs two readings; reporting zero would be a lie that
        // looks like an idle machine.
        let times = parse_cpu(PROC_STAT).unwrap();
        assert_eq!(cpu_percent(CpuTimes::default(), times), None);
    }

    #[test]
    fn cpu_is_the_share_of_the_delta_that_was_not_idle() {
        let previous = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let current = CpuTimes {
            total: 1100,
            idle: 850,
        };
        // 100 ticks passed, 50 of them idle.
        assert_eq!(cpu_percent(previous, current), Some(50.0));
    }

    #[test]
    fn counters_going_backwards_are_not_a_delta() {
        // /proc/stat should never go backwards, but a suspended machine and a
        // reset counter both look like this and neither is 4 billion percent.
        let previous = CpuTimes {
            total: 1000,
            idle: 500,
        };
        assert_eq!(
            cpu_percent(
                previous,
                CpuTimes {
                    total: 900,
                    idle: 400
                }
            ),
            None
        );
    }

    #[test]
    fn memory_used_is_total_minus_available() {
        let text = "\
MemTotal:       16000000 kB
MemFree:          500000 kB
MemAvailable:    4000000 kB
Buffers:          100000 kB
";
        // Not MemFree: a machine with a warm cache would read as almost full.
        let used = parse_memory(text).expect("should parse");
        assert!((used - 75.0).abs() < 0.001, "{used}");
    }

    #[test]
    fn a_meminfo_without_a_total_is_absent_rather_than_zero() {
        assert_eq!(parse_memory("MemAvailable: 100 kB\n"), None);
        assert_eq!(parse_memory(""), None);
    }

    const PROC_NET: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000000    1000    0    0    0     0          0         0  1000000    1000    0    0    0     0       0          0
  eth0:  500000     500    0    0    0     0          0         0   250000     250    0    0    0     0       0          0
  wlan0: 300000     300    0    0    0     0          0         0   150000     150    0    0    0     0       0          0
";

    #[test]
    fn loopback_is_not_network_activity() {
        let (rx, tx) = parse_network(PROC_NET);
        // eth0 + wlan0, and none of lo's megabyte.
        assert_eq!(rx, 800_000);
        assert_eq!(tx, 400_000);
    }

    #[test]
    fn an_interface_going_away_does_not_report_exabytes() {
        // The totals are sums over the interfaces that exist now, so a VPN
        // dropping makes them go backwards. Unsigned subtraction wrapped and
        // the bar showed some exabytes per second for one sample.
        assert_eq!(rate(800_000, 400_000, 2.0), 0.0);
        // The ordinary case still works.
        assert_eq!(rate(400_000, 800_000, 2.0), 200_000.0);
        // And a zero interval is not a division.
        assert_eq!(rate(0, 800_000, 0.0), 0.0);
    }

    #[test]
    fn a_real_sample_reads_this_machine() {
        // Not asserting values — they are whatever the machine is doing — but
        // the shapes have to be right, and /proc is not going to be missing.
        let mut status = Status::default();
        let first = status.sample();
        assert!(first.cpu.is_none(), "the first sample cannot know");
        assert!(first.memory.is_some(), "meminfo should be readable");
        assert!(first.disk_total > 0.0, "the root filesystem has a size");

        // Long enough for the clock to advance. Back to back, /proc/stat has
        // not changed and there is genuinely nothing to compare — which is the
        // code being right rather than wrong.
        std::thread::sleep(std::time::Duration::from_millis(60));
        let second = status.sample();
        let cpu = second.cpu.expect("the second sample can compare");
        assert!((0.0..=100.0).contains(&cpu), "cpu out of range: {cpu}");
    }

    #[test]
    fn an_unconfigured_sample_wants_no_volume_or_mounts() {
        // The default `Status` is the no-widget bar: it must not stat extra
        // mounts or spawn wpctl when nobody asked, and the extra fields stay
        // empty rather than lying.
        let mut status = Status::default();
        let sample = status.sample();
        assert!(sample.mounts.is_empty());
        assert_eq!(sample.volume, None);
        assert_eq!(sample.muted, None);
        assert_eq!(sample.mic_volume, None);
        assert_eq!(sample.mic_muted, None);
    }

    #[test]
    fn configured_mounts_are_each_reported() {
        // Every path a widget asked for comes back as its own entry, in
        // order; the root mount is only omitted when nobody asked for it.
        let mut status = Status::default();
        status.configure(
            vec!["/home".to_owned(), "/mnt/data".to_owned()],
            false,
            false,
            true,
        );
        let sample = status.sample();
        let paths: Vec<&str> = sample.mounts.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["/home", "/mnt/data"]);
        // Mounts that exist have a size; the default disk numbers still
        // describe the root mount regardless.
        assert!(sample.disk_total > 0.0);
    }

    #[test]
    fn the_displayed_reduction_rounds_where_the_shell_rounds() {
        // The signature has to be finer than the display in the same places
        // and no coarser; otherwise a change the bar shows would be compared
        // equal and never sent. These are the shell's own rules.
        assert_eq!(display_percent(Some(12.4)), Some(12));
        assert_eq!(display_percent(Some(-1.0)), None, "the absent sentinel");
        assert_eq!(display_bytes(0.0), DisplayedBytes::Zero);
        assert_eq!(
            display_bytes(1024.0),
            DisplayedBytes::Tenths {
                unit: 1,
                tenths: 10
            },
            "1.0K"
        );
        assert_eq!(
            display_bytes(10.0 * 1024.0),
            DisplayedBytes::Rounded {
                unit: 1,
                rounded: 10
            },
            "10K"
        );
        assert_eq!(
            display_bytes(1.5 * 1024.0),
            DisplayedBytes::Tenths {
                unit: 1,
                tenths: 15
            },
            "1.5K"
        );
        assert_ne!(
            display_bytes(9.9 * 1024.0),
            display_bytes(10.0 * 1024.0),
            "9.9K and 10K are different widget text"
        );
        assert_eq!(display_audio(Some(0.444), Some(false)), Some((44, false)));
        assert_eq!(display_audio(Some(-1.0), Some(true)), None);
        assert_eq!(display_audio(Some(0.66), None), Some((66, false)));
    }

    #[test]
    fn a_periodic_sample_only_goes_out_when_the_bar_would_change() {
        let mut status = Status::default();
        status.configure(Vec::new(), true, false, true);
        let base = Sample {
            cpu: Some(12.4),
            memory: Some(33.2),
            load: [1.234, 0.5, 0.25],
            net_rx: 1024.4,
            net_tx: 0.0,
            disk_free: 1024.0 * 1024.0 * 1024.0,
            volume: Some(0.444),
            muted: Some(false),
            ..Sample::default()
        };
        let now = Instant::now();

        // Nothing published yet: the first sample always goes out.
        assert!(status.should_publish_periodic(&base, Duration::from_secs(30), now));

        // Every difference here is below what its widget draws.
        let below = Sample {
            cpu: Some(12.1),
            load: [1.2344, 0.5, 0.25],
            net_rx: 1024.1,
            volume: Some(0.4441),
            ..base.clone()
        };
        assert!(
            !status.should_publish_periodic(&below, Duration::from_secs(30), now),
            "rounding differences the bar cannot show are not an update"
        );

        // ... but a displayed percentage moving by one is.
        let changed = Sample {
            cpu: Some(12.6),
            ..below.clone()
        };
        assert!(status.should_publish_periodic(&changed, Duration::from_secs(30), now));

        // A mute glyph changing is a change even at the same percentage.
        let muted = Sample {
            muted: Some(true),
            ..changed.clone()
        };
        assert!(status.should_publish_periodic(&muted, Duration::from_secs(30), now));

        // Every published sample also refreshes the baseline, so repeating it
        // is not an update.
        assert!(!status.should_publish_periodic(&muted, Duration::from_secs(30), now));
    }

    #[test]
    fn a_heartbeat_publishes_an_unchanged_sample_for_a_fresh_shell() {
        let mut status = Status::default();
        status.configure(Vec::new(), false, false, true);
        let sample = Sample {
            cpu: Some(10.0),
            ..Sample::default()
        };
        let now = Instant::now();
        let heartbeat = Duration::from_secs(30);

        assert!(status.should_publish_periodic(&sample, heartbeat, now));
        assert!(!status.should_publish_periodic(&sample, heartbeat, now + Duration::from_secs(29)));
        assert!(status.should_publish_periodic(&sample, heartbeat, now + Duration::from_secs(30)));
    }

    #[test]
    fn a_forced_publish_ignores_an_unchanged_sample() {
        let mut status = Status::default();
        status.configure(Vec::new(), false, false, true);
        let sample = Sample {
            cpu: Some(10.0),
            ..Sample::default()
        };
        let now = Instant::now();
        assert!(status.should_publish_periodic(&sample, Duration::from_secs(30), now));
        status.force_publish();
        assert!(
            status.should_publish_periodic(&sample, Duration::from_secs(30), now),
            "a shell that just connected has no state, changed or not"
        );
    }

    #[test]
    fn a_bar_with_no_status_widget_does_not_sample() {
        // Only an explicit no-status bar turns this off; the default is the
        // sampler that has always run.
        assert!(Status::default().wanted());
        let mut status = Status::default();
        status.configure(Vec::new(), false, false, false);
        assert!(!status.wanted());
    }

    #[test]
    fn a_volume_change_is_a_step_then_a_toggle() {
        // What `status.volume` turns into, without running any of it: the sign
        // goes after the unit, a fall is spelled as a positive number down, and
        // a message that asks for both gets the change before the toggle so the
        // measurement that follows describes the end state.
        assert_eq!(
            audio_args(SINK, Some(5), false),
            [["set-volume", SINK, "5%+"]]
        );
        assert_eq!(
            audio_args(SINK, Some(-5), false),
            [["set-volume", SINK, "5%-"]]
        );
        assert_eq!(
            audio_args(SOURCE, Some(3), true),
            [
                vec!["set-volume", SOURCE, "3%+"],
                vec!["set-mute", SOURCE, "toggle"],
            ]
        );
        assert_eq!(audio_args(SINK, None, true), [["set-mute", SINK, "toggle"]]);
    }

    #[test]
    fn a_volume_message_that_asks_for_nothing_runs_nothing() {
        // A zero step with no toggle is still two forks if it is passed on.
        // Nothing is queued, and the caller is told it need not sample.
        assert!(audio_args(SINK, Some(0), false).is_empty());
        assert!(audio_args(SINK, None, false).is_empty());
        let mut status = Status::default();
        assert!(
            !status.set_audio(SINK, Some(0), false),
            "nothing to do, so nothing to report"
        );
    }

    #[test]
    fn only_a_sample_frees_the_slot_the_next_sample_asks_in() {
        // A change's answer arrives through the same channel as a sample's, and
        // must not be mistaken for one: the tick that queued a sample is still
        // waiting for it, and a second queued behind it would have the
        // compositor asking faster than the worker answers.
        let mut status = Status {
            asked: true,
            ..Status::default()
        };

        let changed = Slow {
            volume: Some(0.4),
            sampled: false,
            ..Slow::default()
        };
        assert!(
            status.absorb(changed).0,
            "the volume moved, so the bar hears"
        );
        assert!(
            status.asked,
            "the sample that was asked for is still coming"
        );

        let sampled = Slow {
            volume: Some(0.4),
            sampled: true,
            ..Slow::default()
        };
        assert!(
            !status.absorb(sampled).0,
            "nothing moved since the last answer"
        );
        assert!(!status.asked, "and now the next tick may ask again");
    }

    #[test]
    fn a_worker_answers_without_the_sampler_waiting() {
        // With a worker the slow half is asked for and not waited for: the
        // sample that asks reports the previous answer, and the one after the
        // answer arrives reports it. What the compositor must never do is block
        // on a `statvfs` or a `wpctl` on its own thread.
        let mut loop_ = smithay::reexports::calloop::EventLoop::<Option<Slow>>::try_new()
            .expect("an event loop");
        let (sender, source) = smithay::reexports::calloop::channel::channel::<Slow>();
        loop_
            .handle()
            .insert_source(source, |event, _, taken: &mut Option<Slow>| {
                if let smithay::reexports::calloop::channel::Event::Msg(slow) = event {
                    *taken = Some(slow);
                }
            })
            .expect("the channel source");

        let mut status = Status::default();
        status.start(sender).expect("the worker should start");
        status.configure(vec!["/".to_owned()], false, false, true);

        // Nothing has come back yet, so this reports the seed — the root
        // filesystem, and no mounts.
        let first = status.sample();
        assert!(first.disk_total > 0.0, "the seed has the root filesystem");
        assert!(first.mounts.is_empty(), "the worker has not answered yet");

        let mut taken = None;
        while taken.is_none() {
            loop_
                .dispatch(std::time::Duration::from_secs(2), &mut taken)
                .expect("the loop should run");
        }
        status.absorb(taken.expect("the worker should answer"));

        let second = status.sample();
        let paths: Vec<&str> = second.mounts.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["/"]);
    }

    #[test]
    fn only_a_changed_volume_is_worth_a_message_of_its_own() {
        // A disk figure that drifted by a block is not; it goes out with the
        // next tick. A volume that changed is, because a scroll on the bar is
        // answered by re-sampling and has to show at once.
        let mut status = Status::default();
        assert!(!status.absorb(Slow::default()).0, "nothing changed");
        assert!(
            !status
                .absorb(Slow {
                    disk_free: 1.0,
                    ..Slow::default()
                })
                .0,
            "a disk figure waits for the tick"
        );
        assert!(
            status
                .absorb(Slow {
                    disk_free: 1.0,
                    volume: Some(0.5),
                    ..Slow::default()
                })
                .0,
            "a volume does not"
        );
    }

    #[test]
    fn the_sink_parses_volume_and_mute() {
        let (volume, muted) = parse_sink("Volume: 0.45\nMuted: no\n");
        assert_eq!(volume, Some(0.45));
        assert_eq!(muted, Some(false));
    }

    #[test]
    fn a_muted_sink_still_reports_a_volume() {
        // Muted and quiet are different facts; `0.00` is a volume like any
        // other.
        let (volume, muted) = parse_sink("Volume: 0.00\nMuted: yes\n");
        assert_eq!(volume, Some(0.0));
        assert_eq!(muted, Some(true));
    }

    #[test]
    fn a_modern_muted_sink_marks_mute_on_the_volume_line() {
        // Modern wpctl folds the mute state into the volume line as a `[...]`
        // marker and prints no `Muted:` line. The trailing marker must not
        // swallow the volume, or a right-clicked mute would blank the widget.
        let (volume, muted) = parse_sink("Volume: 0.60 [MUTED]\n");
        assert_eq!(volume, Some(0.6));
        assert_eq!(muted, Some(true));
        // Unmuted there is no marker at all.
        let (volume, muted) = parse_sink("Volume: 0.60\n");
        assert_eq!(volume, Some(0.6));
        assert_eq!(muted, None);
    }

    #[test]
    fn a_sink_that_says_nothing_is_absent() {
        // No default sink: wpctl prints an error instead of a Volume line.
        let (volume, muted) = parse_sink("No default audio sink found.\n");
        assert_eq!(volume, None);
        assert_eq!(muted, None);
    }

    #[test]
    fn brightnessctl_readback_becomes_a_bounded_fraction() {
        assert_eq!(
            parse_brightness("intel_backlight,backlight,48000,47%,102400\n"),
            Some(0.47)
        );
        assert_eq!(
            parse_brightness("Updated device 'panel':\nCurrent brightness: 1024 (100%)\n"),
            Some(1.0)
        );
        assert_eq!(parse_brightness("no percentage here"), None);
    }

    #[test]
    fn requested_feedback_survives_worker_delivery() {
        let mut status = Status {
            slow: Slow {
                volume: Some(0.4),
                muted: Some(false),
                mic_volume: Some(0.3),
                mic_muted: Some(true),
                mounts: vec![MountUsage {
                    path: "/home".to_owned(),
                    free: 10.0,
                    total: 20.0,
                }],
                ..Slow::default()
            },
            ..Status::default()
        };
        let (changed, osd) = status.absorb(Slow {
            brightness: Some(0.62),
            osd: Some(viewport_ipc::event::StatusOsd::Brightness),
            ..Slow::default()
        });
        assert!(!changed, "brightness is not an audio-widget update");
        assert_eq!(osd, Some(viewport_ipc::event::StatusOsd::Brightness));
        assert_eq!(status.slow.brightness, Some(0.62));
        assert_eq!(status.slow.volume, Some(0.4));
        assert_eq!(status.slow.mic_volume, Some(0.3));
        assert_eq!(status.slow.mounts[0].path, "/home");
    }

    #[test]
    fn relative_steps_handle_the_full_signed_range() {
        assert_eq!(relative_percent(5), "5%+");
        assert_eq!(relative_percent(-5), "5%-");
        assert_eq!(relative_percent(i32::MIN), "2147483648%-");
    }
}
