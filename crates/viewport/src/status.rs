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

use std::time::Instant;

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
}

/// Free and total bytes on one mount a bar widget asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct MountUsage {
    pub path: String,
    pub free: f64,
    pub total: f64,
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

/// Samples the machine, keeping what it needs to turn counters into rates.
#[derive(Default)]
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
}

impl Status {
    /// Which mounts, and which audio nodes' volumes, to sample on the next
    /// ticks.
    ///
    /// Called from config application; `Status::default()` wants none of them,
    /// which is a bar with no widgets and thus no reason to stat extra mounts
    /// or spawn `wpctl` every two seconds.
    pub fn configure(&mut self, mounts: Vec<String>, want_volume: bool, want_mic: bool) {
        self.mounts = mounts;
        self.want_volume = want_volume;
        self.want_mic = want_mic;
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
        let (free, total) = disk_usage("/");
        sample.disk_free = free;
        sample.disk_total = total;

        // The extra mounts a bar widget asked about, one entry per configured
        // path. A widget that names `/` reports the root mount as its own
        // entry, which is that widget's business — the default disk module
        // next to it is a separate element.
        for path in &self.mounts {
            let (free, total) = disk_usage(path);
            sample.mounts.push(MountUsage {
                path: path.clone(),
                free,
                total,
            });
        }

        if self.want_volume {
            let (volume, muted) = audio_state("@DEFAULT_AUDIO_SINK@");
            sample.volume = volume;
            sample.muted = muted;
        }

        if self.want_mic {
            let (volume, muted) = audio_state("@DEFAULT_AUDIO_SOURCE@");
            sample.mic_volume = volume;
            sample.mic_muted = muted;
        }

        self.at = Some(now);
        sample
    }
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
        );
        let sample = status.sample();
        let paths: Vec<&str> = sample.mounts.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["/home", "/mnt/data"]);
        // Mounts that exist have a size; the default disk numbers still
        // describe the root mount regardless.
        assert!(sample.disk_total > 0.0);
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
}
