use serde::Serialize;

pub const CPU_MODES: [&str; 8] = ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal"];

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuTimes {
    pub cpu: String,
    /// Ticks per mode, ordered as CPU_MODES.
    pub ticks: [u64; 8],
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.ticks.iter().sum()
    }
    pub fn idle(&self) -> u64 {
        self.ticks[3] + self.ticks[4]
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub total: CpuTimes,
    pub cpus: Vec<CpuTimes>,
    pub boot_time: u64,
    pub ctxt: u64,
    pub forks: u64,
    pub procs_running: u64,
    pub procs_blocked: u64,
    pub intr: u64,
}

pub fn parse_stat(s: &str) -> Stat {
    let mut st = Stat::default();
    for line in s.lines() {
        let mut it = line.split_ascii_whitespace();
        let Some(key) = it.next() else { continue };
        if let Some(rest) = key.strip_prefix("cpu") {
            let vals: Vec<u64> = it.map(|v| v.parse().unwrap_or(0)).collect();
            let mut c = CpuTimes { cpu: rest.to_string(), ..Default::default() };
            for (i, v) in vals.iter().take(8).enumerate() {
                c.ticks[i] = *v;
            }
            c.guest = vals.get(8).copied().unwrap_or(0);
            c.guest_nice = vals.get(9).copied().unwrap_or(0);
            if rest.is_empty() {
                st.total = c;
            } else {
                st.cpus.push(c);
            }
            continue;
        }
        let v: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match key {
            "btime" => st.boot_time = v,
            "ctxt" => st.ctxt = v,
            "processes" => st.forks = v,
            "procs_running" => st.procs_running = v,
            "procs_blocked" => st.procs_blocked = v,
            "intr" => st.intr = v,
            _ => {}
        }
    }
    st
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Load {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

pub fn parse_loadavg(s: &str) -> Option<Load> {
    let mut it = s.split_ascii_whitespace();
    Some(Load {
        load1: it.next()?.parse().ok()?,
        load5: it.next()?.parse().ok()?,
        load15: it.next()?.parse().ok()?,
    })
}

/// Returns (node_exporter field name, value in bytes or raw count).
pub fn parse_meminfo(s: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let Some((k, rest)) = line.split_once(':') else { continue };
        let mut parts = rest.split_ascii_whitespace();
        let Some(v) = parts.next().and_then(|v| v.parse::<u64>().ok()) else { continue };
        let v = if parts.next() == Some("kB") { v * 1024 } else { v };
        let name = k.replace('(', "_").replace(')', "");
        out.push((name, v));
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiskStat {
    pub device: String,
    pub reads: u64,
    pub reads_merged: u64,
    pub read_sectors: u64,
    pub read_ms: u64,
    pub writes: u64,
    pub writes_merged: u64,
    pub write_sectors: u64,
    pub write_ms: u64,
    pub io_now: u64,
    pub io_ms: u64,
    pub io_weighted_ms: u64,
    pub discards: u64,
    pub discard_sectors: u64,
    pub flushes: u64,
}

pub fn parse_diskstats(s: &str) -> Vec<(u32, u32, DiskStat)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let d = DiskStat {
            device: f[2].to_string(),
            reads: n(3),
            reads_merged: n(4),
            read_sectors: n(5),
            read_ms: n(6),
            writes: n(7),
            writes_merged: n(8),
            write_sectors: n(9),
            write_ms: n(10),
            io_now: n(11),
            io_ms: n(12),
            io_weighted_ms: n(13),
            discards: n(14),
            discard_sectors: n(16),
            flushes: n(18),
        };
        out.push((n(0) as u32, n(1) as u32, d));
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetDev {
    pub device: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

pub fn parse_net_dev(s: &str) -> Vec<NetDev> {
    let mut out = Vec::new();
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let f: Vec<u64> = rest.split_ascii_whitespace().map(|v| v.parse().unwrap_or(0)).collect();
        if f.len() < 16 {
            continue;
        }
        out.push(NetDev {
            device: name.trim().to_string(),
            rx_bytes: f[0],
            rx_packets: f[1],
            rx_errs: f[2],
            rx_drop: f[3],
            tx_bytes: f[8],
            tx_packets: f[9],
            tx_errs: f[10],
            tx_drop: f[11],
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub readonly: bool,
}

pub fn parse_mounts(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() < 4 {
                return None;
            }
            Some(Mount {
                device: unescape_octal(f[0]),
                mountpoint: unescape_octal(f[1]),
                fstype: f[2].to_string(),
                readonly: f[3].split(',').any(|o| o == "ro"),
            })
        })
        .collect()
}

fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// utime + stime in ticks and RSS in pages from /proc/<pid>/stat.
pub fn parse_pid_stat(s: &str) -> Option<(u64, u64, u64)> {
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let rss: u64 = f.get(21)?.parse().ok()?;
    Some((utime, stime, rss))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 2 30 1000 5 1 2 0 7 0\ncpu0 50 1 15 500 3 1 1 0 0 0\ncpu1 50 1 15 500 2 0 1 0 0 0\nintr 12345 0 0\nctxt 999\nbtime 1700000000\nprocesses 4242\nprocs_running 3\nprocs_blocked 1\n";

    #[test]
    fn stat() {
        let s = parse_stat(STAT);
        assert_eq!(s.cpus.len(), 2);
        assert_eq!(s.total.ticks, [100, 2, 30, 1000, 5, 1, 2, 0]);
        assert_eq!(s.total.guest, 7);
        assert_eq!(s.cpus[1].cpu, "1");
        assert_eq!(s.boot_time, 1700000000);
        assert_eq!(s.ctxt, 999);
        assert_eq!(s.forks, 4242);
        assert_eq!(s.procs_running, 3);
        assert_eq!(s.total.idle(), 1005);
    }

    #[test]
    fn loadavg() {
        let l = parse_loadavg("0.52 1.10 2.00 3/811 12345\n").unwrap();
        assert_eq!((l.load1, l.load5, l.load15), (0.52, 1.10, 2.00));
        assert!(parse_loadavg("").is_none());
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo("MemTotal:       16384 kB\nActive(anon):     100 kB\nHugePages_Total:       4\n");
        assert_eq!(m[0], ("MemTotal".to_string(), 16384 * 1024));
        assert_eq!(m[1], ("Active_anon".to_string(), 100 * 1024));
        assert_eq!(m[2], ("HugePages_Total".to_string(), 4));
    }

    #[test]
    fn diskstats() {
        let d = parse_diskstats("   8       0 sda 100 2 800 50 200 4 1600 70 0 120 130 0 0 0 0 0 0\n 259 0 nvme0n1 1 0 8 1 2 0 16 1 0 2 2\n");
        assert_eq!(d.len(), 2);
        let (maj, min, s) = &d[0];
        assert_eq!((*maj, *min), (8, 0));
        assert_eq!(s.device, "sda");
        assert_eq!(s.read_sectors, 800);
        assert_eq!(s.write_sectors, 1600);
        assert_eq!(s.io_ms, 120);
        assert_eq!(d[1].2.discards, 0);
    }

    #[test]
    fn net_dev() {
        let s = "Inter-|   Receive |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n    lo: 2980529   52164    0    0    0     0          0         0  2980529   52164    0    0    0     0       0          0\n  eth0: 293066351 1773063 1 2 0 0 0 0 1424161045 1162039 3 4 0 0 0 0\n";
        let n = parse_net_dev(s);
        assert_eq!(n.len(), 2);
        assert_eq!(n[1].device, "eth0");
        assert_eq!(n[1].rx_bytes, 293066351);
        assert_eq!(n[1].tx_bytes, 1424161045);
        assert_eq!((n[1].rx_errs, n[1].rx_drop, n[1].tx_errs, n[1].tx_drop), (1, 2, 3, 4));
    }

    #[test]
    fn mounts() {
        let m = parse_mounts("/dev/sda1 / ext4 rw,relatime 0 0\nserver:/x /mnt/my\\040share nfs4 ro,vers=4 0 0\n");
        assert_eq!(m[0], Mount { device: "/dev/sda1".into(), mountpoint: "/".into(), fstype: "ext4".into(), readonly: false });
        assert_eq!(m[1].mountpoint, "/mnt/my share");
        assert!(m[1].readonly);
    }

    #[test]
    fn pid_stat() {
        let s = "1234 (my (weird) proc) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 4 0 100 50000000 2048 18446744073709551615";
        assert_eq!(parse_pid_stat(s), Some((250, 50, 2048)));
    }
}
