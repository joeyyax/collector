mod deadline;
pub mod gpu;
pub mod hwmon;
pub mod procfs;

use crate::config::Config;
use crate::metrics::{Kind, LabelSet, MetricSet};
use crate::util::{self, matches, rate, read};
use procfs::{CpuTimes, DiskStat, NetDev, CPU_MODES};
use regex_lite::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostSnapshot {
    pub host: String,
    pub timestamp: f64,
    pub boot_time: u64,
    pub uptime_seconds: f64,
    pub cpu: CpuSummary,
    pub load: procfs::Load,
    pub memory: Memory,
    pub filesystems: Vec<Filesystem>,
    pub disks: Vec<Disk>,
    pub networks: Vec<Network>,
    pub sensors: Vec<hwmon::Sensor>,
    /// Served beside the host, not inside it; see `api`.
    #[serde(skip)]
    pub gpus: Vec<gpu::Gpu>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuSummary {
    pub count: usize,
    /// Busy share of all CPUs since the previous collection, 0..1.
    pub usage_ratio: Option<f64>,
    pub modes: HashMap<&'static str, f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub per_cpu: Vec<PerCpu>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PerCpu {
    pub cpu: String,
    pub usage_ratio: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub buffers_bytes: u64,
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Filesystem {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub avail_bytes: u64,
    pub used_ratio: f64,
    pub files: u64,
    pub files_free: u64,
    pub readonly: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Disk {
    pub device: String,
    pub read_bytes_total: u64,
    pub written_bytes_total: u64,
    pub read_bytes_per_second: Option<f64>,
    pub written_bytes_per_second: Option<f64>,
    /// Share of wall time the device was busy, 0..1.
    pub busy_ratio: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Network {
    pub device: String,
    pub receive_bytes_total: u64,
    pub transmit_bytes_total: u64,
    pub receive_bytes_per_second: Option<f64>,
    pub transmit_bytes_per_second: Option<f64>,
    pub receive_errors_total: u64,
    pub transmit_errors_total: u64,
}

struct Prev {
    at: f64,
    total: CpuTimes,
    cpus: Vec<CpuTimes>,
    disks: HashMap<String, DiskStat>,
    nets: HashMap<String, NetDev>,
}

pub struct HostCollector {
    proc: PathBuf,
    sys: PathBuf,
    rootfs: PathBuf,
    host: String,
    per_cpu: bool,
    hwmon: bool,
    gpu: Option<gpu::Watcher>,
    fs_types: Option<Regex>,
    fs_mounts: Option<Regex>,
    disk_exclude: Option<Regex>,
    net_exclude: Option<Regex>,
    tck: f64,
    prev: Option<Prev>,
    statfs: deadline::Deadline,
    statfs_timeout: std::time::Duration,
}

const H_CPU: &str = "Seconds the CPUs spent in each mode.";
const H_GUEST: &str = "Seconds the CPUs spent in guests (VMs) for each mode.";

impl HostCollector {
    pub fn new(cfg: &Config, gpu: Option<gpu::Watcher>) -> Self {
        let h = &cfg.host_metrics;
        Self {
            proc: cfg.paths.proc.clone(),
            sys: cfg.paths.sys.clone(),
            rootfs: cfg.paths.rootfs.clone(),
            host: cfg.host.clone(),
            per_cpu: h.per_cpu,
            hwmon: h.hwmon,
            gpu,
            fs_types: util::regex(&h.filesystem_exclude_types),
            fs_mounts: util::regex(&h.filesystem_exclude_mounts),
            disk_exclude: util::regex(&h.disk_exclude),
            net_exclude: util::regex(&h.network_exclude),
            tck: util::clk_tck(),
            prev: None,
            statfs: deadline::Deadline::default(),
            statfs_timeout: h.statfs_timeout,
        }
    }

    pub fn collect(&mut self, m: &mut MetricSet) -> HostSnapshot {
        let now = util::now_secs();
        let mut snap = HostSnapshot { host: self.host.clone(), timestamp: now, ..Default::default() };
        let prev = self.prev.take();
        let dt = prev.as_ref().map(|p| now - p.at).unwrap_or(0.0);

        let stat = procfs::parse_stat(&read(self.proc.join("stat")).unwrap_or_default());
        self.cpu(m, &stat, prev.as_ref(), &mut snap);

        if let Some(l) = read(self.proc.join("loadavg")).and_then(|s| procfs::parse_loadavg(&s)) {
            m.gauge("node_load1", "1m load average.", &[], l.load1);
            m.gauge("node_load5", "5m load average.", &[], l.load5);
            m.gauge("node_load15", "15m load average.", &[], l.load15);
            snap.load = l;
        }

        self.memory(m, &mut snap);
        self.filesystems(m, &mut snap);
        let disks = self.disks(m, prev.as_ref(), dt, &mut snap);
        let nets = self.networks(m, prev.as_ref(), dt, &mut snap);
        self.kernel(m, now);

        if self.hwmon {
            snap.sensors = hwmon::collect(&self.sys);
            emit_sensors(m, &snap.sensors);
        }
        if let Some(w) = &self.gpu {
            let g = w.latest();
            emit_gpus(m, &g);
            snap.gpus = g;
        }

        self.prev = Some(Prev { at: now, total: stat.total, cpus: stat.cpus, disks, nets });
        snap
    }

    fn cpu(&self, m: &mut MetricSet, stat: &procfs::Stat, prev: Option<&Prev>, snap: &mut HostSnapshot) {
        let t = self.tck;
        let cpus: Vec<CpuTimes> = if self.per_cpu { stat.cpus.clone() } else { vec![CpuTimes { cpu: "all".into(), ..stat.total.clone() }] };
        for c in &cpus {
            for (i, mode) in CPU_MODES.iter().enumerate() {
                m.counter("node_cpu_seconds_total", H_CPU, &[("cpu", &c.cpu), ("mode", mode)], c.ticks[i] as f64 / t);
            }
            m.counter("node_cpu_guest_seconds_total", H_GUEST, &[("cpu", &c.cpu), ("mode", "user")], c.guest as f64 / t);
            m.counter("node_cpu_guest_seconds_total", H_GUEST, &[("cpu", &c.cpu), ("mode", "nice")], c.guest_nice as f64 / t);
        }
        m.gauge("node_boot_time_seconds", "Boot time, seconds since epoch.", &[], stat.boot_time as f64);
        m.counter("node_context_switches_total", "Context switches.", &[], stat.ctxt as f64);
        m.counter("node_forks_total", "Forks.", &[], stat.forks as f64);
        m.counter("node_intr_total", "Interrupts serviced.", &[], stat.intr as f64);
        m.gauge("node_procs_running", "Processes in runnable state.", &[], stat.procs_running as f64);
        m.gauge("node_procs_blocked", "Processes blocked on I/O.", &[], stat.procs_blocked as f64);

        snap.boot_time = stat.boot_time;
        snap.uptime_seconds = (snap.timestamp - stat.boot_time as f64).max(0.0);
        snap.cpu.count = stat.cpus.len();
        if let Some(p) = prev {
            let dtot = stat.total.total().saturating_sub(p.total.total()) as f64;
            if dtot > 0.0 {
                let didle = stat.total.idle().saturating_sub(p.total.idle()) as f64;
                snap.cpu.usage_ratio = Some(((dtot - didle) / dtot).clamp(0.0, 1.0));
                for (i, mode) in CPU_MODES.iter().enumerate() {
                    let d = stat.total.ticks[i].saturating_sub(p.total.ticks[i]) as f64;
                    snap.cpu.modes.insert(mode, d / dtot);
                }
            }
            if self.per_cpu {
                for (c, pc) in stat.cpus.iter().zip(p.cpus.iter()) {
                    let dtot = c.total().saturating_sub(pc.total()) as f64;
                    let didle = c.idle().saturating_sub(pc.idle()) as f64;
                    let usage = (dtot > 0.0).then(|| ((dtot - didle) / dtot).clamp(0.0, 1.0));
                    snap.cpu.per_cpu.push(PerCpu { cpu: c.cpu.clone(), usage_ratio: usage });
                }
            }
        }
    }

    fn memory(&self, m: &mut MetricSet, snap: &mut HostSnapshot) {
        let info = procfs::parse_meminfo(&read(self.proc.join("meminfo")).unwrap_or_default());
        let mut get = HashMap::new();
        for (k, v) in &info {
            let name = if k.starts_with("HugePages_") { format!("node_memory_{k}") } else { format!("node_memory_{k}_bytes") };
            m.push(&name, "Memory information field from /proc/meminfo.", Kind::Gauge, &[], *v as f64);
            get.insert(k.as_str(), *v);
        }
        let g = |k: &str| get.get(k).copied().unwrap_or(0);
        let mem = &mut snap.memory;
        mem.total_bytes = g("MemTotal");
        mem.available_bytes = g("MemAvailable");
        mem.free_bytes = g("MemFree");
        mem.buffers_bytes = g("Buffers");
        mem.cached_bytes = g("Cached");
        mem.used_bytes = mem.total_bytes.saturating_sub(mem.available_bytes);
        mem.swap_total_bytes = g("SwapTotal");
        mem.swap_free_bytes = g("SwapFree");
        mem.swap_used_bytes = mem.swap_total_bytes.saturating_sub(mem.swap_free_bytes);
    }

    fn filesystems(&self, m: &mut MetricSet, snap: &mut HostSnapshot) {
        let text = read(self.proc.join("1/mounts")).or_else(|| read(self.proc.join("self/mounts"))).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let mounts: Vec<_> = procfs::parse_mounts(&text)
            .into_iter()
            .filter(|mt| !matches(&self.fs_types, &mt.fstype) && !matches(&self.fs_mounts, &mt.mountpoint) && seen.insert(mt.mountpoint.clone()))
            .collect();
        let jobs = mounts
            .iter()
            .map(|mt| {
                let path = self.rootfs.join(mt.mountpoint.trim_start_matches('/'));
                let job: Box<dyn FnOnce() -> Result<StatVfs, i32> + Send> = Box::new(move || statvfs(&path));
                (mt.mountpoint.clone(), job)
            })
            .collect();
        let mut results = self.statfs.run(jobs, self.statfs_timeout);
        m.gauge("collector_filesystems_hung", "Mounts whose statfs call has not returned.", &[], self.statfs.stuck() as f64);
        for mt in mounts {
            let st = match results.remove(&mt.mountpoint) {
                Some(deadline::Outcome::Done(Ok(st))) => st,
                // Not visible under rootfs, e.g. a mount from another namespace.
                Some(deadline::Outcome::Done(Err(e))) if e == libc::ENOENT || e == libc::ENOTDIR => continue,
                _ => {
                    m.gauge("node_filesystem_device_error", "Whether statfs failed for this filesystem.", &[("device", &mt.device), ("mountpoint", &mt.mountpoint), ("fstype", &mt.fstype)], 1.0);
                    continue;
                }
            };
            let l: [(&str, &str); 3] = [("device", &mt.device), ("mountpoint", &mt.mountpoint), ("fstype", &mt.fstype)];
            m.gauge("node_filesystem_size_bytes", "Filesystem size in bytes.", &l, st.size as f64);
            m.gauge("node_filesystem_free_bytes", "Filesystem free space in bytes.", &l, st.free as f64);
            m.gauge("node_filesystem_avail_bytes", "Filesystem space available to non-root users in bytes.", &l, st.avail as f64);
            m.gauge("node_filesystem_files", "Filesystem total file nodes.", &l, st.files as f64);
            m.gauge("node_filesystem_files_free", "Filesystem total free file nodes.", &l, st.files_free as f64);
            m.gauge("node_filesystem_readonly", "Filesystem read-only status.", &l, if mt.readonly { 1.0 } else { 0.0 });
            m.gauge("node_filesystem_device_error", "Whether statfs failed for this filesystem.", &l, 0.0);
            let used = st.size.saturating_sub(st.free);
            let denom = used + st.avail;
            snap.filesystems.push(Filesystem {
                device: mt.device,
                mountpoint: mt.mountpoint,
                fstype: mt.fstype,
                size_bytes: st.size,
                free_bytes: st.free,
                avail_bytes: st.avail,
                used_ratio: if denom > 0 { used as f64 / denom as f64 } else { 0.0 },
                files: st.files,
                files_free: st.files_free,
                readonly: mt.readonly,
            });
        }
    }

    fn disks(&self, m: &mut MetricSet, prev: Option<&Prev>, dt: f64, snap: &mut HostSnapshot) -> HashMap<String, DiskStat> {
        let mut out = HashMap::new();
        for (_, _, d) in procfs::parse_diskstats(&read(self.proc.join("diskstats")).unwrap_or_default()) {
            if matches(&self.disk_exclude, &d.device) {
                continue;
            }
            let l: [(&str, &str); 1] = [("device", &d.device)];
            m.counter("node_disk_reads_completed_total", "Reads completed.", &l, d.reads as f64);
            m.counter("node_disk_reads_merged_total", "Reads merged.", &l, d.reads_merged as f64);
            m.counter("node_disk_read_bytes_total", "Bytes read.", &l, (d.read_sectors * 512) as f64);
            m.counter("node_disk_read_time_seconds_total", "Seconds spent reading.", &l, d.read_ms as f64 / 1000.0);
            m.counter("node_disk_writes_completed_total", "Writes completed.", &l, d.writes as f64);
            m.counter("node_disk_writes_merged_total", "Writes merged.", &l, d.writes_merged as f64);
            m.counter("node_disk_written_bytes_total", "Bytes written.", &l, (d.write_sectors * 512) as f64);
            m.counter("node_disk_write_time_seconds_total", "Seconds spent writing.", &l, d.write_ms as f64 / 1000.0);
            m.gauge("node_disk_io_now", "I/Os in progress.", &l, d.io_now as f64);
            m.counter("node_disk_io_time_seconds_total", "Seconds spent doing I/Os.", &l, d.io_ms as f64 / 1000.0);
            m.counter("node_disk_io_time_weighted_seconds_total", "Weighted seconds spent doing I/Os.", &l, d.io_weighted_ms as f64 / 1000.0);
            let p = prev.and_then(|p| p.disks.get(&d.device));
            snap.disks.push(Disk {
                device: d.device.clone(),
                read_bytes_total: d.read_sectors * 512,
                written_bytes_total: d.write_sectors * 512,
                read_bytes_per_second: p.and_then(|p| rate(p.read_sectors * 512, d.read_sectors * 512, dt)),
                written_bytes_per_second: p.and_then(|p| rate(p.write_sectors * 512, d.write_sectors * 512, dt)),
                busy_ratio: p.and_then(|p| rate(p.io_ms, d.io_ms, dt)).map(|v| (v / 1000.0).min(1.0)),
            });
            out.insert(d.device.clone(), d);
        }
        out
    }

    fn networks(&self, m: &mut MetricSet, prev: Option<&Prev>, dt: f64, snap: &mut HostSnapshot) -> HashMap<String, NetDev> {
        let mut out = HashMap::new();
        for n in procfs::parse_net_dev(&read(self.proc.join("1/net/dev")).or_else(|| read(self.proc.join("net/dev"))).unwrap_or_default()) {
            if matches(&self.net_exclude, &n.device) {
                continue;
            }
            emit_net(m, "node_network", None, &[("device", &n.device)], &n);
            self.netclass(m, &n.device);
            let p = prev.and_then(|p| p.nets.get(&n.device));
            snap.networks.push(Network {
                device: n.device.clone(),
                receive_bytes_total: n.rx_bytes,
                transmit_bytes_total: n.tx_bytes,
                receive_bytes_per_second: p.and_then(|p| rate(p.rx_bytes, n.rx_bytes, dt)),
                transmit_bytes_per_second: p.and_then(|p| rate(p.tx_bytes, n.tx_bytes, dt)),
                receive_errors_total: n.rx_errs,
                transmit_errors_total: n.tx_errs,
            });
            out.insert(n.device.clone(), n);
        }
        out
    }
}

impl HostCollector {
    /// Link state, speed and MTU from sysfs; a missing or unreadable file leaves its series out.
    fn netclass(&self, m: &mut MetricSet, dev: &str) {
        let dir = self.sys.join("class/net").join(dev);
        let l: [(&str, &str); 1] = [("device", dev)];
        if let Some(state) = read(dir.join("operstate")) {
            m.gauge("node_network_up", "Value is 1 if operstate is 'up', 0 otherwise.", &l, if state.trim() == "up" { 1.0 } else { 0.0 });
        }
        if let Some(v) = read(dir.join("speed")).and_then(|s| procfs::parse_speed_bytes(&s)) {
            m.gauge("node_network_speed_bytes", "Network device speed in bytes per second.", &l, v as f64);
        }
        if let Some(v) = util::read_u64(dir.join("mtu")) {
            m.gauge("node_network_mtu_bytes", "Network device MTU in bytes.", &l, v as f64);
        }
    }

    /// Kernel identity, clock, vmstat, PSI, file handles and conntrack. Each file that's missing leaves its series out.
    fn kernel(&self, m: &mut MetricSet, now: f64) {
        let sysctl = |k: &str| read(self.proc.join("sys").join(k)).map(|s| s.trim().to_string());
        if let (Some(sysname), Some(release), Some(version)) = (sysctl("kernel/ostype"), sysctl("kernel/osrelease"), sysctl("kernel/version")) {
            let domain = sysctl("kernel/domainname").unwrap_or_else(|| "(none)".into());
            let l = [("domainname", domain.as_str()), ("machine", std::env::consts::ARCH), ("nodename", &self.host), ("release", &release), ("sysname", &sysname), ("version", &version)];
            m.gauge("node_uname_info", "Labeled system information as provided by the uname system call.", &l, 1.0);
        }
        m.gauge("node_time_seconds", "System time in seconds since epoch (1970).", &[], now);
        for (k, v) in procfs::parse_vmstat(&read(self.proc.join("vmstat")).unwrap_or_default()) {
            m.push(&format!("node_vmstat_{k}"), "/proc/vmstat information field.", Kind::Untyped, &[], v as f64);
        }
        for res in ["cpu", "io", "memory", "irq"] {
            let Some(text) = read(self.proc.join("pressure").join(res)) else { continue };
            let p = procfs::parse_pressure(&text);
            if let Some(v) = p.some_usec {
                m.counter(&format!("node_pressure_{res}_waiting_seconds_total"), "Total time in seconds that processes have waited for this resource.", &[], v as f64 / 1e6);
            }
            if let Some(v) = p.full_usec {
                m.counter(&format!("node_pressure_{res}_stalled_seconds_total"), "Total time in seconds no process could make progress due to this resource.", &[], v as f64 / 1e6);
            }
        }
        if let Some((alloc, max)) = sysctl("fs/file-nr").and_then(|s| procfs::parse_file_nr(&s)) {
            m.gauge("node_filefd_allocated", "File descriptor statistics: allocated.", &[], alloc as f64);
            m.gauge("node_filefd_maximum", "File descriptor statistics: maximum.", &[], max as f64);
        }
        // /proc/sys/net follows the reader's network namespace; /proc/1/net is the host's.
        let entries = read(self.proc.join("1/net/stat/nf_conntrack"))
            .and_then(|s| procfs::parse_conntrack_stat(&s))
            .or_else(|| sysctl("net/netfilter/nf_conntrack_count").and_then(|s| s.parse().ok()));
        if let Some(v) = entries {
            m.gauge("node_nf_conntrack_entries", "Number of currently allocated flow entries for connection tracking.", &[], v as f64);
        }
        if let Some(v) = sysctl("net/netfilter/nf_conntrack_max").and_then(|s| s.parse::<u64>().ok()) {
            m.gauge("node_nf_conntrack_entries_limit", "Maximum size of connection tracking table.", &[], v as f64);
        }
    }
}

/// Emits node_exporter or cAdvisor style interface counters under `prefix`.
pub fn emit_net(m: &mut MetricSet, prefix: &str, base: Option<&LabelSet>, labels: &[(&str, &str)], n: &NetDev) {
    let cadvisor = prefix.starts_with("container");
    let (errs, drop) = if cadvisor { ("errors_total", "packets_dropped_total") } else { ("errs_total", "drop_total") };
    let rows: [(&str, &str, u64, &'static str); 8] = [
        ("receive", "bytes_total", n.rx_bytes, "Bytes received."),
        ("receive", "packets_total", n.rx_packets, "Packets received."),
        ("receive", errs, n.rx_errs, "Receive errors."),
        ("receive", drop, n.rx_drop, "Receive drops."),
        ("transmit", "bytes_total", n.tx_bytes, "Bytes transmitted."),
        ("transmit", "packets_total", n.tx_packets, "Packets transmitted."),
        ("transmit", errs, n.tx_errs, "Transmit errors."),
        ("transmit", drop, n.tx_drop, "Transmit drops."),
    ];
    for (dir, suffix, v, help) in rows {
        let name = format!("{prefix}_{dir}_{suffix}");
        match base {
            Some(b) => m.push_with(&name, help, Kind::Counter, b, labels, v as f64),
            None => m.counter(&name, help, labels, v as f64),
        }
    }
}

fn emit_sensors(m: &mut MetricSet, sensors: &[hwmon::Sensor]) {
    let mut chips = std::collections::BTreeMap::new();
    for s in sensors {
        chips.insert(s.chip.as_str(), s.chip_name.as_str());
        let l: [(&str, &str); 2] = [("chip", &s.chip), ("sensor", &s.sensor)];
        match s.kind {
            hwmon::SensorKind::Temp => {
                m.gauge("node_hwmon_temp_celsius", "Hardware monitor for temperature (input).", &l, s.value);
                if let Some(v) = s.max {
                    m.gauge("node_hwmon_temp_max_celsius", "Hardware monitor for temperature (max).", &l, v);
                }
                if let Some(v) = s.crit {
                    m.gauge("node_hwmon_temp_crit_celsius", "Hardware monitor for temperature (crit).", &l, v);
                }
            }
            hwmon::SensorKind::Fan => m.gauge("node_hwmon_fan_rpm", "Hardware monitor for fan revolutions per minute (input).", &l, s.value),
        }
        if let Some(label) = &s.label {
            m.gauge("node_hwmon_sensor_label", "Label for given chip and sensor.", &[("chip", &s.chip), ("sensor", &s.sensor), ("label", label)], 1.0);
        }
    }
    for (chip, name) in chips {
        m.gauge("node_hwmon_chip_names", "Annotation metric for human-readable chip names.", &[("chip", chip), ("chip_name", name)], 1.0);
    }
}

fn emit_gpus(m: &mut MetricSet, gpus: &[gpu::Gpu]) {
    for g in gpus {
        let idx = g.index.to_string();
        m.gauge("nvidia_smi_gpu_info", "GPU identity.", &[("uuid", &g.uuid), ("name", &g.name), ("index", &idx)], 1.0);
        let l: [(&str, &str); 1] = [("uuid", &g.uuid)];
        let rows: [(&str, &'static str, Option<f64>); 7] = [
            ("nvidia_smi_temperature_gpu", "GPU core temperature in Celsius.", g.temperature_celsius),
            ("nvidia_smi_utilization_gpu_ratio", "GPU utilization, 0..1.", g.utilization_ratio),
            ("nvidia_smi_utilization_memory_ratio", "GPU memory controller utilization, 0..1.", g.memory_utilization_ratio),
            ("nvidia_smi_memory_used_bytes", "GPU memory used.", g.memory_used_bytes),
            ("nvidia_smi_memory_total_bytes", "GPU memory total.", g.memory_total_bytes),
            ("nvidia_smi_power_draw_watts", "GPU power draw.", g.power_draw_watts),
            ("nvidia_smi_fan_speed_ratio", "GPU fan speed, 0..1.", g.fan_speed_ratio),
        ];
        for (name, help, v) in rows {
            if let Some(v) = v {
                m.gauge(name, help, &l, v);
            }
        }
    }
}

struct StatVfs {
    size: u64,
    free: u64,
    avail: u64,
    files: u64,
    files_free: u64,
}

// Casts matter on 32-bit targets.
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &std::path::Path) -> Result<StatVfs, i32> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    // SAFETY: c is NUL-terminated and st is a valid out pointer.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let bs = st.f_frsize as u64;
    Ok(StatVfs {
        size: st.f_blocks as u64 * bs,
        free: st.f_bfree as u64 * bs,
        avail: st.f_bavail as u64 * bs,
        files: st.f_files as u64,
        files_free: st.f_ffree as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn collector(root: &std::path::Path) -> HostCollector {
        let mut cfg = Config { host: "web-1".into(), ..Default::default() };
        cfg.paths.proc = root.join("proc");
        cfg.paths.sys = root.join("sys");
        cfg.paths.rootfs = root.join("rootfs");
        cfg.host_metrics.hwmon = false;
        HostCollector::new(&cfg, None)
    }

    #[test]
    fn tier_one_metrics_from_fixtures() {
        let root = std::env::temp_dir().join(format!("collector-host-{}", std::process::id()));
        write(&root, "proc/sys/kernel/ostype", "Linux\n");
        write(&root, "proc/sys/kernel/osrelease", "6.8.0-45-generic\n");
        write(&root, "proc/sys/kernel/version", "#45-Ubuntu SMP PREEMPT_DYNAMIC Fri Aug 30 12:02:04 UTC 2024\n");
        write(&root, "proc/vmstat", "pgmajfault 20931\noom_kill 3\npswpin 0\npswpout 0\n");
        write(&root, "proc/pressure/cpu", "some avg10=0.00 avg60=0.00 avg300=0.00 total=2500000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
        write(&root, "proc/pressure/memory", "some avg10=0.00 avg60=0.00 avg300=0.00 total=1000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=500\n");
        write(&root, "proc/sys/fs/file-nr", "9408\t0\t1048576\n");
        write(&root, "proc/sys/net/netfilter/nf_conntrack_count", "77\n");
        write(&root, "proc/sys/net/netfilter/nf_conntrack_max", "262144\n");
        write(&root, "proc/1/net/dev", "Inter-|\n face |\n  eth0: 1 2 0 0 0 0 0 0 3 4 0 0 0 0 0 0\n  wg0: 1 2 0 0 0 0 0 0 3 4 0 0 0 0 0 0\n");
        write(&root, "sys/class/net/eth0/operstate", "up\n");
        write(&root, "sys/class/net/eth0/speed", "1000\n");
        write(&root, "sys/class/net/eth0/mtu", "1500\n");
        write(&root, "sys/class/net/wg0/operstate", "unknown\n");
        write(&root, "sys/class/net/wg0/mtu", "1420\n");
        let mut m = MetricSet::new();
        collector(&root).collect(&mut m);
        let _ = std::fs::remove_dir_all(&root);
        let text = m.render(&[], crate::config::Compat::None);
        let line = |prefix: &str| text.lines().find(|l| l.starts_with(prefix)).map(str::to_string);

        assert_eq!(
            line("node_uname_info").unwrap(),
            format!(r##"node_uname_info{{domainname="(none)",machine="{}",nodename="web-1",release="6.8.0-45-generic",sysname="Linux",version="#45-Ubuntu SMP PREEMPT_DYNAMIC Fri Aug 30 12:02:04 UTC 2024"}} 1"##, std::env::consts::ARCH)
        );
        assert!(line("node_time_seconds ").is_some());
        assert_eq!(line("node_vmstat_oom_kill").unwrap(), "node_vmstat_oom_kill 3");
        assert!(text.contains("# TYPE node_vmstat_pgmajfault untyped\n"));
        assert_eq!(line("node_pressure_cpu_waiting_seconds_total").unwrap(), "node_pressure_cpu_waiting_seconds_total 2.5");
        assert_eq!(line("node_pressure_memory_stalled_seconds_total").unwrap(), "node_pressure_memory_stalled_seconds_total 0.0005");
        assert_eq!(line("node_filefd_maximum").unwrap(), "node_filefd_maximum 1048576");
        assert_eq!(line("node_nf_conntrack_entries ").unwrap(), "node_nf_conntrack_entries 77");
        assert_eq!(line("node_nf_conntrack_entries_limit").unwrap(), "node_nf_conntrack_entries_limit 262144");
        assert_eq!(line("node_network_up{device=\"eth0\"}").unwrap(), r#"node_network_up{device="eth0"} 1"#);
        assert_eq!(line("node_network_up{device=\"wg0\"}").unwrap(), r#"node_network_up{device="wg0"} 0"#);
        assert_eq!(line("node_network_speed_bytes").unwrap(), r#"node_network_speed_bytes{device="eth0"} 125000000"#);
        assert_eq!(line("node_network_mtu_bytes{device=\"wg0\"}").unwrap(), r#"node_network_mtu_bytes{device="wg0"} 1420"#);
        // No io or irq PSI files, no wg0 speed: left out rather than zero.
        assert!(!text.contains("node_pressure_io") && !text.contains("node_pressure_irq") && !text.contains(r#"speed_bytes{device="wg0"}"#));
    }

    #[test]
    fn missing_files_leave_series_out() {
        let root = std::env::temp_dir().join(format!("collector-host-empty-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut m = MetricSet::new();
        collector(&root).collect(&mut m);
        let _ = std::fs::remove_dir_all(&root);
        let names: Vec<&str> = m.families.iter().filter(|f| !f.samples.is_empty()).map(|f| f.name.as_str()).collect();
        for absent in ["node_uname_info", "node_pressure_cpu_waiting_seconds_total", "node_vmstat_oom_kill", "node_filefd_allocated", "node_nf_conntrack_entries", "node_network_up"] {
            assert!(!names.contains(&absent), "{absent} present without its file");
        }
        assert!(names.contains(&"node_time_seconds"));
    }
}
