use crate::host::procfs::{parse_pressure, Pressure};
use crate::util::{read, read_limit, read_u64};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

pub fn parse_cpu_stat(s: &str) -> CpuStat {
    let kv = parse_kv(s);
    let g = |k: &str| kv.get(k).copied().unwrap_or(0);
    CpuStat {
        usage_usec: g("usage_usec"),
        user_usec: g("user_usec"),
        system_usec: g("system_usec"),
        nr_periods: g("nr_periods"),
        nr_throttled: g("nr_throttled"),
        throttled_usec: g("throttled_usec"),
    }
}

/// Parses `key value` lines (cpu.stat, memory.stat, memory.events).
pub fn parse_kv(s: &str) -> HashMap<&str, u64> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_ascii_whitespace();
            Some((it.next()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// cpu.max: "<quota|max> <period>". Returns (quota_usec, period_usec).
pub fn parse_cpu_max(s: &str) -> (Option<u64>, u64) {
    let mut it = s.split_ascii_whitespace();
    let quota = it.next().and_then(|q| q.parse().ok());
    let period = it.next().and_then(|p| p.parse().ok()).unwrap_or(100_000);
    (quota, period)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct IoStat {
    pub major: u32,
    pub minor: u32,
    pub rbytes: u64,
    pub wbytes: u64,
    pub rios: u64,
    pub wios: u64,
}

pub fn parse_io_stat(s: &str) -> Vec<IoStat> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_ascii_whitespace();
            let (maj, min) = it.next()?.split_once(':')?;
            let mut io = IoStat { major: maj.parse().ok()?, minor: min.parse().ok()?, ..Default::default() };
            for kv in it {
                let Some((k, v)) = kv.split_once('=') else { continue };
                let v = v.parse().unwrap_or(0);
                match k {
                    "rbytes" => io.rbytes = v,
                    "wbytes" => io.wbytes = v,
                    "rios" => io.rios = v,
                    "wios" => io.wios = v,
                    _ => {}
                }
            }
            Some(io)
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Raw {
    pub cpu: CpuStat,
    pub cpu_quota_usec: Option<u64>,
    pub cpu_period_usec: u64,
    pub mem_current: u64,
    pub mem_peak: Option<u64>,
    pub mem_max: Option<u64>,
    pub mem_swap: Option<u64>,
    pub mem_anon: u64,
    pub mem_file: u64,
    pub mem_inactive_file: u64,
    pub mem_active_file: u64,
    pub mem_file_mapped: u64,
    pub mem_kernel: u64,
    pub oom_kills: u64,
    pub io: Vec<IoStat>,
    pub pids: u64,
    pub pids_max: Option<u64>,
    /// PSI by resource; a resource whose `*.pressure` file is missing is absent.
    pub pressure: Vec<(&'static str, Pressure)>,
}

pub const PRESSURE: [&str; 3] = ["cpu", "memory", "io"];

impl Raw {
    /// cAdvisor's working set: usage minus inactive file cache.
    pub fn working_set(&self) -> u64 {
        self.mem_current.saturating_sub(self.mem_inactive_file)
    }
}

pub fn read_cgroup(dir: &Path) -> Option<Raw> {
    let cpu = parse_cpu_stat(&read(dir.join("cpu.stat"))?);
    let (quota, period) = read(dir.join("cpu.max")).map(|s| parse_cpu_max(&s)).unwrap_or((None, 100_000));
    let mem_stat = read(dir.join("memory.stat")).unwrap_or_default();
    let ms = parse_kv(&mem_stat);
    let events = read(dir.join("memory.events")).unwrap_or_default();
    let ev = parse_kv(&events);
    let g = |m: &HashMap<&str, u64>, k: &str| m.get(k).copied().unwrap_or(0);
    Some(Raw {
        cpu,
        cpu_quota_usec: quota,
        cpu_period_usec: period,
        mem_current: read_u64(dir.join("memory.current")).unwrap_or(0),
        mem_peak: read_u64(dir.join("memory.peak")),
        mem_max: read_limit(dir.join("memory.max")).flatten(),
        mem_swap: read_u64(dir.join("memory.swap.current")),
        mem_anon: g(&ms, "anon"),
        mem_file: g(&ms, "file"),
        mem_inactive_file: g(&ms, "inactive_file"),
        mem_active_file: g(&ms, "active_file"),
        mem_file_mapped: g(&ms, "file_mapped"),
        mem_kernel: g(&ms, "kernel"),
        oom_kills: g(&ev, "oom_kill"),
        io: read(dir.join("io.stat")).map(|s| parse_io_stat(&s)).unwrap_or_default(),
        pids: read_u64(dir.join("pids.current")).unwrap_or(0),
        pids_max: read_limit(dir.join("pids.max")).flatten(),
        pressure: PRESSURE.iter().filter_map(|r| Some((*r, parse_pressure(&read(dir.join(format!("{r}.pressure")))?)))).collect(),
    })
}

/// Finds the container's cgroup under the systemd or cgroupfs layout.
pub fn find_dir(cgroup_root: &Path, id: &str, proc: &Path, pid: u32) -> Option<PathBuf> {
    let candidates = [
        format!("system.slice/docker-{id}.scope"),
        format!("docker/{id}"),
        format!("system.slice/containerd.service/docker-{id}.scope"),
    ];
    for c in candidates {
        let p = cgroup_root.join(c);
        if p.join("cpu.stat").exists() {
            return Some(p);
        }
    }
    if pid > 0 {
        let line = read(proc.join(format!("{pid}/cgroup")))?;
        let rel = line.lines().find_map(|l| l.strip_prefix("0::"))?;
        let rel = rel.trim().trim_start_matches("/..").trim_start_matches('/');
        let rel = rel.split("/../").last().unwrap_or(rel);
        let p = cgroup_root.join(rel);
        if p.join("cpu.stat").exists() {
            return Some(p);
        }
    }
    None
}

/// The first Docker container id in a `/proc/<pid>/cgroup` file that `known` accepts. Handles cgroup v1, v2 and namespace-relative paths (`0::/../docker-<id>.scope`).
pub fn container_id(text: &str, known: impl Fn(&str) -> bool) -> Option<&str> {
    text.lines()
        .filter_map(|l| l.splitn(3, ':').nth(2))
        .flat_map(|p| p.split('/'))
        .map(|c| {
            let c = c.strip_prefix("docker-").unwrap_or(c);
            c.strip_suffix(".scope").unwrap_or(c)
        })
        .find(|c| c.len() == 64 && c.bytes().all(|b| b.is_ascii_hexdigit()) && known(c))
}

/// Maps "major:minor" to block device names from /sys/dev/block.
pub fn block_device_name(sys: &Path, major: u32, minor: u32) -> Option<String> {
    let uevent = read(sys.join(format!("dev/block/{major}:{minor}/uevent")))?;
    uevent.lines().find_map(|l| l.strip_prefix("DEVNAME=")).map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_stat() {
        let c = parse_cpu_stat("usage_usec 369839272\nuser_usec 234214277\nsystem_usec 135624994\nnice_usec 0\nnr_periods 10\nnr_throttled 3\nthrottled_usec 4500\nnr_bursts 0\n");
        assert_eq!(c.usage_usec, 369839272);
        assert_eq!(c.user_usec, 234214277);
        assert_eq!(c.nr_periods, 10);
        assert_eq!(c.nr_throttled, 3);
        assert_eq!(c.throttled_usec, 4500);
    }

    #[test]
    fn cpu_max() {
        assert_eq!(parse_cpu_max("max 100000\n"), (None, 100000));
        assert_eq!(parse_cpu_max("50000 100000\n"), (Some(50000), 100000));
    }

    #[test]
    fn io_stat() {
        let io = parse_io_stat("259:0 rbytes=1024 wbytes=2048 rios=1 wios=2 dbytes=0 dios=0\n8:0 rbytes=5 wbytes=6 rios=7 wios=8\n");
        assert_eq!(io.len(), 2);
        assert_eq!(io[0], IoStat { major: 259, minor: 0, rbytes: 1024, wbytes: 2048, rios: 1, wios: 2 });
        assert_eq!(io[1].wios, 8);
    }

    #[test]
    fn reads_fixture_dir() {
        let dir = std::env::temp_dir().join(format!("collector-cg-{}", std::process::id()));
        let cg = dir.join("system.slice/docker-abc.scope");
        std::fs::create_dir_all(&cg).unwrap();
        std::fs::write(cg.join("cpu.stat"), "usage_usec 2000000\nuser_usec 1500000\nsystem_usec 500000\nnr_periods 0\nnr_throttled 0\nthrottled_usec 0\n").unwrap();
        std::fs::write(cg.join("cpu.max"), "200000 100000\n").unwrap();
        std::fs::write(cg.join("memory.current"), "1000\n").unwrap();
        std::fs::write(cg.join("memory.peak"), "1500\n").unwrap();
        std::fs::write(cg.join("memory.max"), "max\n").unwrap();
        std::fs::write(cg.join("memory.stat"), "anon 600\nfile 400\ninactive_file 300\nactive_file 100\nfile_mapped 50\nkernel 20\n").unwrap();
        std::fs::write(cg.join("memory.events"), "low 0\nhigh 0\nmax 0\noom 2\noom_kill 1\n").unwrap();
        std::fs::write(cg.join("pids.current"), "7\n").unwrap();
        std::fs::write(cg.join("pids.max"), "max\n").unwrap();
        std::fs::write(cg.join("cpu.pressure"), "some avg10=0.00 avg60=0.00 avg300=0.00 total=3000000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=1000000\n").unwrap();
        std::fs::write(cg.join("memory.pressure"), "some avg10=0.00 avg60=0.00 avg300=0.00 total=10\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=5\n").unwrap();
        let found = find_dir(&dir, "abc", Path::new("/nonexistent"), 0).unwrap();
        let r = read_cgroup(&found).unwrap();
        assert_eq!(r.cpu.usage_usec, 2_000_000);
        assert_eq!(r.cpu_quota_usec, Some(200000));
        assert_eq!(r.mem_max, None);
        assert_eq!(r.mem_peak, Some(1500));
        assert_eq!(r.working_set(), 700);
        assert_eq!(r.oom_kills, 1);
        assert_eq!(r.pids, 7);
        // No io.pressure in the fixture, as on kernels or hosts without PSI.
        assert_eq!(r.pressure, vec![("cpu", Pressure { some_usec: Some(3_000_000), full_usec: Some(1_000_000) }), ("memory", Pressure { some_usec: Some(10), full_usec: Some(5) })]);
        assert!(find_dir(&dir, "zzz", Path::new("/nonexistent"), 0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn container_id_from_proc_cgroup() {
        let a = "3f2a9c4e1b7d8a6f5e4c3b2a1908f7e6d5c4b3a29180f7e6d5c4b3a2918070f6";
        let b = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";
        let known = |id: &str| id == a || id == b;
        assert_eq!(container_id(&format!("0::/system.slice/docker-{a}.scope\n"), known), Some(a));
        assert_eq!(container_id(&format!("0::/../docker-{a}.scope\n"), known), Some(a));
        assert_eq!(container_id(&format!("0::/docker/{b}/init.scope\n"), known), Some(b));
        assert_eq!(container_id(&format!("12:memory:/docker/{b}\n0::/docker/{b}\n"), known), Some(b));
        // Nested: the outer container is the one Docker knows.
        assert_eq!(container_id(&format!("0::/docker/{a}/docker/{}\n", "f".repeat(64)), known), Some(a));
        assert_eq!(container_id("0::/user.slice/user-1000.slice/session-3.scope\n", known), None);
        assert_eq!(container_id(&format!("0::/system.slice/docker-{}.scope\n", "e".repeat(64)), known), None);
    }
}
