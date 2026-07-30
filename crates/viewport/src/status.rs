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
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sample {
    /// Percentage, or `None` before there are two samples to compare.
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub load: [f64; 3],
    /// Bytes per second since the last sample.
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_free: f64,
    pub disk_total: f64,
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
}


impl Status {
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
        let previous = CpuTimes { total: 1000, idle: 800 };
        let current = CpuTimes { total: 1100, idle: 850 };
        // 100 ticks passed, 50 of them idle.
        assert_eq!(cpu_percent(previous, current), Some(50.0));
    }

    #[test]
    fn counters_going_backwards_are_not_a_delta() {
        // /proc/stat should never go backwards, but a suspended machine and a
        // reset counter both look like this and neither is 4 billion percent.
        let previous = CpuTimes { total: 1000, idle: 500 };
        assert_eq!(cpu_percent(previous, CpuTimes { total: 900, idle: 400 }), None);
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
}
