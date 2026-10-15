pub mod cgroup;
pub mod docker;

use crate::config::{self, Config};
use crate::host::{emit_net, gpu, procfs};
use crate::metrics::{sanitize_label_name, Kind, LabelSet, MetricSet};
use crate::util::{self, log, rate, read};
use docker::Docker;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub pid: u32,
    pub network_mode: String,
    pub started_at: String,
    pub created: String,
    /// Every Docker label, for filters; `labels` holds those kept by `containers.labels` or `containers.api_labels`.
    #[serde(skip)]
    pub all_labels: BTreeMap<String, String>,
    #[serde(skip)]
    pub log_path: String,
    #[serde(skip)]
    pub log_driver: String,
    #[serde(skip)]
    pub tty: bool,
    #[serde(skip)]
    pub cgroup: Option<PathBuf>,
    #[serde(skip)]
    pub metric_labels: LabelSet,
}

impl Container {
    pub fn label(&self, k: &str) -> Option<&str> {
        self.labels.get(k).map(String::as_str)
    }
    pub fn compose_project(&self) -> Option<&str> {
        self.label("com.docker.compose.project")
    }
    pub fn compose_service(&self) -> Option<&str> {
        self.label("com.docker.compose.service")
    }
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}

/// Running containers, kept current from the Docker API and its event stream.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<String, Arc<Container>>>>,
    pub docker_ok: Arc<AtomicBool>,
    synced: Arc<AtomicBool>,
}

impl Registry {
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())), docker_ok: Arc::new(AtomicBool::new(false)), synced: Arc::new(AtomicBool::new(false)) }
    }

    /// True once the first full sync has finished.
    pub fn synced(&self) -> bool {
        self.synced.load(Ordering::Relaxed)
    }

    pub fn all(&self) -> Vec<Arc<Container>> {
        let mut v: Vec<_> = self.inner.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Starts the sync thread and the event watcher.
    pub fn start(&self, cfg: &Config) {
        let docker = Docker::new(&cfg.paths.docker_socket);
        let (tx, rx) = channel::<Option<String>>();
        spawn_events(docker.clone(), tx.clone());
        let me = self.clone();
        let cgroot = cfg.paths.sys.join("fs/cgroup");
        let proc = cfg.paths.proc.clone();
        let rules = cfg.containers.clone();
        let resync = cfg.containers.resync;
        std::thread::Builder::new()
            .name("registry".into())
            .spawn(move || {
                // Events re-inspect only their containers; a full list costs the daemon about a CPU-second on a busy host.
                let mut stale: Vec<String> = Vec::new();
                let mut full = true;
                loop {
                    if full {
                        me.sync(&docker, &cgroot, &proc, &rules, &stale);
                    } else {
                        me.update(&docker, &cgroot, &proc, &rules, &stale);
                    }
                    stale.clear();
                    full = false;
                    match rx.recv_timeout(resync) {
                        Ok(id) => {
                            full |= id.is_none();
                            stale.extend(id);
                            // Coalesce bursts, e.g. a compose up.
                            std::thread::sleep(Duration::from_millis(300));
                            while let Ok(id) = rx.try_recv() {
                                full |= id.is_none();
                                stale.extend(id);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => full = true,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn registry");
    }

    fn sync(&self, docker: &Docker, cgroot: &Path, proc: &Path, rules: &config::Containers, stale: &[String]) {
        let list = match docker.list() {
            Ok(l) => {
                self.docker_ok.store(true, Ordering::Relaxed);
                l
            }
            Err(e) => {
                self.docker_ok.store(false, Ordering::Relaxed);
                log::warn!("{e}");
                return;
            }
        };
        let current = self.inner.read().unwrap().clone();
        let mut next = HashMap::new();
        for s in list.into_iter().filter(|s| s.state == "running") {
            if let Some(c) = current.get(&s.id) {
                if !stale.contains(&s.id) && c.pid > 0 {
                    next.insert(s.id.clone(), c.clone());
                    continue;
                }
            }
            match docker.inspect(&s.id) {
                Ok(i) => {
                    let c = build(i, cgroot, proc, rules);
                    next.insert(c.id.clone(), Arc::new(c));
                }
                Err(e) => log::debug!("{e}"),
            }
        }
        log::debug!("registry: {} running containers", next.len());
        *self.inner.write().unwrap() = next;
        self.synced.store(true, Ordering::Relaxed);
    }
}

impl Registry {
    /// Re-inspects only the containers an event named, dropping any that are gone or no longer running.
    fn update(&self, docker: &Docker, cgroot: &Path, proc: &Path, rules: &config::Containers, ids: &[String]) {
        let mut next = self.inner.read().unwrap().clone();
        for id in ids {
            match docker.inspect(id) {
                Ok(i) if counts(&i.state) => {
                    let c = build(i, cgroot, proc, rules);
                    next.insert(c.id.clone(), Arc::new(c));
                }
                _ => {
                    next.remove(id);
                }
            }
        }
        *self.inner.write().unwrap() = next;
    }
}

/// Matches the list's `state == "running"`: paused containers report Running too.
fn counts(s: &docker::InspectState) -> bool {
    s.running && !s.paused
}

fn spawn_events(docker: Docker, tx: Sender<Option<String>>) {
    std::thread::Builder::new()
        .name("docker-events".into())
        .spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                let res = docker.events(|ev| {
                    if ev.kind == "container" && matches!(ev.action.as_str(), "start" | "restart" | "die" | "destroy" | "rename" | "pause" | "unpause") {
                        let _ = tx.send(Some(ev.actor.id));
                    }
                });
                if let Err(e) = res {
                    log::warn!("{e}; reconnecting in {}s", backoff.as_secs());
                }
                // Resync after a reconnect since events may have been missed.
                if tx.send(None).is_err() {
                    return;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        })
        .expect("spawn docker-events");
}

fn build(i: docker::Inspect, cgroot: &Path, proc: &Path, rules: &config::Containers) -> Container {
    let all_labels = i.config.labels.unwrap_or_default();
    let labels: BTreeMap<String, String> =
        all_labels.iter().filter(|(k, _)| keep_label(&rules.labels, k) || keep_label(&rules.api_labels, k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let name = i.name.trim_start_matches('/').to_string();
    let cgroup = cgroup::find_dir(cgroot, &i.id, proc, i.state.pid);
    let id_label = cgroup
        .as_ref()
        .and_then(|p| p.strip_prefix(cgroot).ok())
        .map(|p| format!("/{}", p.display()))
        .unwrap_or_else(|| format!("/docker/{}", i.id));
    let mut metric_labels = vec![("id".to_string(), id_label), ("image".to_string(), i.config.image.clone()), ("name".to_string(), name.clone())];
    for (k, v) in labels.iter().filter(|(k, _)| keep_label(&rules.labels, k)) {
        metric_labels.push((format!("container_label_{}", sanitize_label_name(k)), v.clone()));
    }
    Container {
        id: i.id,
        name,
        image: i.config.image,
        labels,
        all_labels,
        pid: i.state.pid,
        network_mode: i.host_config.network_mode,
        started_at: i.state.started_at,
        created: i.created,
        log_path: i.log_path,
        log_driver: i.host_config.log_config.kind,
        tty: i.config.tty,
        cgroup,
        metric_labels: metric_labels.into(),
    }
}

pub fn keep_label(patterns: &[String], key: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None if p.ends_with('.') => key.starts_with(p.as_str()),
        None => key == p,
    })
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ContainerSnapshot {
    #[serde(flatten)]
    pub meta: Arc<Container>,
    pub cpu: CpuOut,
    pub memory: MemOut,
    pub io: Vec<IoOut>,
    pub network: Vec<NetOut>,
    pub pids: PidsOut,
    pub gpus: Vec<GpuOut>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuOut {
    pub usage_seconds_total: f64,
    /// Cores in use since the previous collection; 1.0 is one full core.
    pub usage_cores: Option<f64>,
    pub limit_cores: Option<f64>,
    pub throttled_periods_total: u64,
    pub throttled_seconds_total: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemOut {
    pub usage_bytes: u64,
    pub working_set_bytes: u64,
    pub peak_bytes: Option<u64>,
    pub limit_bytes: Option<u64>,
    pub rss_bytes: u64,
    pub cache_bytes: u64,
    pub swap_bytes: Option<u64>,
    pub oom_kills_total: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IoOut {
    pub device: String,
    pub read_bytes_total: u64,
    pub write_bytes_total: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetOut {
    pub interface: String,
    pub receive_bytes_total: u64,
    pub transmit_bytes_total: u64,
    pub receive_bytes_per_second: Option<f64>,
    pub transmit_bytes_per_second: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PidsOut {
    pub current: u64,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct GpuOut {
    pub uuid: String,
    pub memory_used_bytes: u64,
}

struct Prev {
    at: f64,
    usage_usec: u64,
    net: HashMap<String, (u64, u64)>,
}

pub struct ContainerCollector {
    registry: Registry,
    proc: PathBuf,
    sys: PathBuf,
    network: bool,
    prev: HashMap<String, Prev>,
    devices: HashMap<(u32, u32), String>,
    gpu: Option<gpu::Watcher>,
}

impl ContainerCollector {
    pub fn new(cfg: &Config, registry: Registry, gpu: Option<gpu::Watcher>) -> Self {
        Self { registry, proc: cfg.paths.proc.clone(), sys: cfg.paths.sys.clone(), network: cfg.containers.network, prev: HashMap::new(), devices: HashMap::new(), gpu }
    }

    pub fn collect(&mut self, m: &mut MetricSet) -> Vec<ContainerSnapshot> {
        let now = util::now_secs();
        let mut out = Vec::new();
        let mut next_prev = HashMap::new();
        let ctrs = self.registry.all();
        let mut gpu_mem = match &self.gpu {
            Some(w) => gpu_by_container(&self.proc, &w.apps(), &ctrs.iter().map(|c| c.id.as_str()).collect()),
            None => HashMap::new(),
        };
        for c in &ctrs {
            let Some(dir) = &c.cgroup else { continue };
            let Some(raw) = cgroup::read_cgroup(dir) else { continue };
            let prev = self.prev.remove(&c.id);
            let prev = prev.as_ref();
            let dt = prev.map(|p| now - p.at).unwrap_or(0.0);
            let l = &c.metric_labels;

            let usage = raw.cpu.usage_usec as f64 / 1e6;
            let cpu = CpuOut {
                usage_seconds_total: usage,
                usage_cores: prev.and_then(|p| rate(p.usage_usec, raw.cpu.usage_usec, dt)).map(|v| v / 1e6),
                limit_cores: raw.cpu_quota_usec.map(|q| q as f64 / raw.cpu_period_usec as f64),
                throttled_periods_total: raw.cpu.nr_throttled,
                throttled_seconds_total: raw.cpu.throttled_usec as f64 / 1e6,
            };
            m.push_with("container_cpu_usage_seconds_total", "Cumulative cpu time consumed in seconds.", Kind::Counter, l, &[("cpu", "total")], usage);
            put(m, "container_cpu_user_seconds_total", "Cumulative user cpu time consumed in seconds.", Kind::Counter, l, raw.cpu.user_usec as f64 / 1e6);
            put(m, "container_cpu_system_seconds_total", "Cumulative system cpu time consumed in seconds.", Kind::Counter, l, raw.cpu.system_usec as f64 / 1e6);
            put(m, "container_cpu_cfs_periods_total", "Number of elapsed enforcement period intervals.", Kind::Counter, l, raw.cpu.nr_periods as f64);
            put(m, "container_cpu_cfs_throttled_periods_total", "Number of throttled period intervals.", Kind::Counter, l, raw.cpu.nr_throttled as f64);
            put(m, "container_cpu_cfs_throttled_seconds_total", "Total time duration the container has been throttled.", Kind::Counter, l, raw.cpu.throttled_usec as f64 / 1e6);
            put(m, "container_spec_cpu_period", "CPU period of the container.", Kind::Gauge, l, raw.cpu_period_usec as f64);
            if let Some(q) = raw.cpu_quota_usec {
                put(m, "container_spec_cpu_quota", "CPU quota of the container.", Kind::Gauge, l, q as f64);
            }

            let memory = MemOut {
                usage_bytes: raw.mem_current,
                working_set_bytes: raw.working_set(),
                peak_bytes: raw.mem_peak,
                limit_bytes: raw.mem_max,
                rss_bytes: raw.mem_anon,
                cache_bytes: raw.mem_file,
                swap_bytes: raw.mem_swap,
                oom_kills_total: raw.oom_kills,
            };
            put(m, "container_memory_usage_bytes", "Current memory usage in bytes, including all memory regardless of when it was accessed.", Kind::Gauge, l, raw.mem_current as f64);
            put(m, "container_memory_working_set_bytes", "Current working set in bytes.", Kind::Gauge, l, raw.working_set() as f64);
            put(m, "container_memory_rss", "Size of RSS in bytes.", Kind::Gauge, l, raw.mem_anon as f64);
            put(m, "container_memory_cache", "Number of bytes of page cache memory.", Kind::Gauge, l, raw.mem_file as f64);
            put(m, "container_memory_mapped_file", "Size of memory mapped files in bytes.", Kind::Gauge, l, raw.mem_file_mapped as f64);
            put(m, "container_memory_kernel_usage", "Size of kernel memory allocated in bytes.", Kind::Gauge, l, raw.mem_kernel as f64);
            put(m, "container_memory_total_inactive_file_bytes", "Current total inactive file in bytes.", Kind::Gauge, l, raw.mem_inactive_file as f64);
            put(m, "container_memory_total_active_file_bytes", "Current total active file in bytes.", Kind::Gauge, l, raw.mem_active_file as f64);
            if let Some(p) = raw.mem_peak {
                put(m, "container_memory_max_usage_bytes", "Maximum memory usage recorded in bytes.", Kind::Gauge, l, p as f64);
            }
            if let Some(s) = raw.mem_swap {
                put(m, "container_memory_swap", "Container swap usage in bytes.", Kind::Gauge, l, s as f64);
            }
            put(m, "container_spec_memory_limit_bytes", "Memory limit for the container.", Kind::Gauge, l, raw.mem_max.unwrap_or(0) as f64);
            put(m, "container_oom_events_total", "Count of out of memory events observed for the container.", Kind::Counter, l, raw.oom_kills as f64);

            let mut io = Vec::new();
            for s in &raw.io {
                let dev = self.device_name(s.major, s.minor);
                let dl: [(&str, &str); 1] = [("device", &dev)];
                m.push_with("container_fs_reads_bytes_total", "Cumulative count of bytes read.", Kind::Counter, l, &dl, s.rbytes as f64);
                m.push_with("container_fs_writes_bytes_total", "Cumulative count of bytes written.", Kind::Counter, l, &dl, s.wbytes as f64);
                m.push_with("container_fs_reads_total", "Cumulative count of reads completed.", Kind::Counter, l, &dl, s.rios as f64);
                m.push_with("container_fs_writes_total", "Cumulative count of writes completed.", Kind::Counter, l, &dl, s.wios as f64);
                io.push(IoOut { device: dev, read_bytes_total: s.rbytes, write_bytes_total: s.wbytes });
            }

            put(m, "container_processes", "Number of processes running inside the container.", Kind::Gauge, l, raw.pids as f64);
            if let Some(max) = raw.pids_max {
                put(m, "container_spec_pids_limit", "Pids limit for the container.", Kind::Gauge, l, max as f64);
            }
            let started = parse_rfc3339(&c.started_at);
            if let Some(t) = started {
                put(m, "container_start_time_seconds", "Start time of the container since unix epoch in seconds.", Kind::Gauge, l, t);
            }
            put(m, "container_last_seen", "Last time a container was seen by the exporter.", Kind::Gauge, l, now.floor());

            let mut network = Vec::new();
            let mut net_prev = HashMap::new();
            if self.network && c.network_mode != "host" && c.pid > 0 {
                let devs = procfs::parse_net_dev(&read(self.proc.join(format!("{}/net/dev", c.pid))).unwrap_or_default());
                for n in devs.into_iter().filter(|n| n.device != "lo") {
                    emit_net(m, "container_network", Some(l), &[("interface", &n.device)], &n);
                    let p = prev.and_then(|p| p.net.get(&n.device));
                    network.push(NetOut {
                        interface: n.device.clone(),
                        receive_bytes_total: n.rx_bytes,
                        transmit_bytes_total: n.tx_bytes,
                        receive_bytes_per_second: p.and_then(|p| rate(p.0, n.rx_bytes, dt)),
                        transmit_bytes_per_second: p.and_then(|p| rate(p.1, n.tx_bytes, dt)),
                    });
                    net_prev.insert(n.device.clone(), (n.rx_bytes, n.tx_bytes));
                }
            }

            let gpus: Vec<GpuOut> = gpu_mem.remove(&c.id).unwrap_or_default().into_iter().map(|(uuid, memory_used_bytes)| GpuOut { uuid, memory_used_bytes }).collect();
            for g in &gpus {
                m.push_with("container_accelerator_memory_used_bytes", "Total accelerator memory allocated.", Kind::Gauge, l, &[("uuid", &g.uuid)], g.memory_used_bytes as f64);
            }

            next_prev.insert(c.id.clone(), Prev { at: now, usage_usec: raw.cpu.usage_usec, net: net_prev });
            out.push(ContainerSnapshot { meta: c.clone(), cpu, memory, io, network, pids: PidsOut { current: raw.pids, limit: raw.pids_max }, gpus });
        }
        self.prev = next_prev;
        out
    }

    fn device_name(&mut self, major: u32, minor: u32) -> String {
        if let Some(n) = self.devices.get(&(major, minor)) {
            return n.clone();
        }
        let name = cgroup::block_device_name(&self.sys, major, minor).map(|n| format!("/dev/{n}")).unwrap_or_else(|| format!("{major}:{minor}"));
        self.devices.insert((major, minor), name.clone());
        name
    }
}

/// GPU memory by container id and GPU uuid, summed over each container's processes. `proc` must be the PID namespace nvidia-smi reports in.
pub fn gpu_by_container(proc: &Path, apps: &[gpu::App], ids: &HashSet<&str>) -> HashMap<String, BTreeMap<String, u64>> {
    let mut out: HashMap<String, BTreeMap<String, u64>> = HashMap::new();
    for a in apps {
        let Some(text) = read(proc.join(format!("{}/cgroup", a.pid))) else { continue };
        let Some(id) = cgroup::container_id(&text, |id| ids.contains(id)) else { continue };
        *out.entry(id.to_string()).or_default().entry(a.uuid.clone()).or_default() += a.memory_used_bytes;
    }
    out
}

fn put(m: &mut MetricSet, name: &str, help: &'static str, kind: Kind, labels: &LabelSet, v: f64) {
    m.push_with(name, help, kind, labels, &[], v);
}

/// Parses Docker's RFC 3339 timestamps (UTC, optional fraction) to epoch seconds.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let (secs, nanos) = rfc3339_parts(s)?;
    Some(secs as f64 + nanos as f64 / 1e9)
}

/// Same, as nanoseconds since the epoch.
pub fn rfc3339_nanos(s: &str) -> Option<u64> {
    let (secs, nanos) = rfc3339_parts(s)?;
    u64::try_from(secs).ok().map(|s| s * 1_000_000_000 + nanos as u64)
}

fn rfc3339_parts(s: &str) -> Option<(i64, u32)> {
    if s.len() < 20 || s.starts_with("0001-") {
        return None;
    }
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let mut nanos = 0u32;
    let mut rest = s.get(19..)?;
    if let Some(r) = rest.strip_prefix('.') {
        let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        let digits = &r[..end.min(9)];
        nanos = format!("{digits:0<9}").parse().unwrap_or(0);
        rest = &r[end..];
    }
    let mut offset = 0i64;
    if rest.len() >= 6 && (rest.starts_with('+') || rest.starts_with('-')) {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        offset = sign * (oh * 3600 + om * 60);
    }
    Some((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset, nanos))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_unpaused_containers_count() {
        let st = |running, paused| docker::InspectState { running, paused, ..Default::default() };
        assert!(counts(&st(true, false)));
        assert!(!counts(&st(true, true)));
        assert!(!counts(&st(false, false)));
    }

    #[test]
    fn label_filter() {
        let p = vec!["com.docker.compose.project".to_string(), "com.example.*".to_string(), "org.example.".to_string()];
        assert!(keep_label(&p, "com.docker.compose.project"));
        assert!(!keep_label(&p, "com.docker.compose.project.working_dir"));
        assert!(keep_label(&p, "com.example.team"));
        assert!(keep_label(&p, "org.example.team"));
        assert!(!keep_label(&p, "com.docker.compose.config-hash"));
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_rfc3339("2026-10-10T12:00:00.5Z"), Some(1791633600.5));
        assert_eq!(parse_rfc3339("2026-10-10T14:00:00+02:00"), Some(1791633600.0));
        assert_eq!(parse_rfc3339("0001-01-01T00:00:00Z"), None);
        assert_eq!(rfc3339_nanos("2026-10-10T12:00:00.123456789Z"), Some(1_791_633_600_123_456_789));
        assert_eq!(rfc3339_nanos("2026-10-10T12:00:00.5Z"), Some(1_791_633_600_500_000_000));
        assert_eq!(rfc3339_nanos("2026-10-10T12:00:00Z"), Some(1_791_633_600_000_000_000));
    }

    #[test]
    fn maps_gpu_processes_to_containers() {
        let (web, shop) = ("a".repeat(64), "b".repeat(64));
        let proc = std::env::temp_dir().join(format!("collector-gpu-{}", std::process::id()));
        for (pid, cg) in [
            (100, format!("0::/system.slice/docker-{web}.scope")),
            (101, format!("0::/system.slice/docker-{web}.scope")),
            (200, format!("0::/../docker-{shop}.scope")),
            (300, "0::/user.slice/user-1000.slice/session-3.scope".to_string()),
        ] {
            std::fs::create_dir_all(proc.join(pid.to_string())).unwrap();
            std::fs::write(proc.join(format!("{pid}/cgroup")), format!("{cg}\n")).unwrap();
        }
        let csv = "100, GPU-1111, 1000\n101, GPU-1111, 24\n101, GPU-2222, 300\n200, GPU-2222, 512\n300, GPU-1111, 64\n404, GPU-1111, 8\n";
        let apps: Vec<gpu::App> = csv.lines().filter_map(gpu::parse_app).collect();
        let ids: HashSet<&str> = [web.as_str(), shop.as_str()].into();
        let got = gpu_by_container(&proc, &apps, &ids);
        let _ = std::fs::remove_dir_all(&proc);
        assert_eq!(got.len(), 2);
        assert_eq!(got[&web], BTreeMap::from([("1111".to_string(), 1024 << 20), ("2222".to_string(), 300 << 20)]));
        assert_eq!(got[&shop], BTreeMap::from([("2222".to_string(), 512 << 20)]));
    }

    #[test]
    fn api_labels_stay_out_of_metrics() {
        let id = "c".repeat(64);
        let root = std::env::temp_dir().join(format!("collector-labels-{}", std::process::id()));
        let cg = root.join(format!("sys/fs/cgroup/system.slice/docker-{id}.scope"));
        std::fs::create_dir_all(&cg).unwrap();
        std::fs::write(cg.join("cpu.stat"), "usage_usec 1000\n").unwrap();
        std::fs::create_dir_all(root.join("proc/4242")).unwrap();
        std::fs::write(root.join("proc/4242/cgroup"), format!("0::/system.slice/docker-{id}.scope\n")).unwrap();
        let mut cfg = Config::default();
        cfg.paths.sys = root.join("sys");
        cfg.paths.proc = root.join("proc");
        cfg.containers.labels = vec!["com.docker.compose.project".into()];
        cfg.containers.api_labels = vec!["com.example.".into()];
        let inspect: docker::Inspect = serde_json::from_value(serde_json::json!({
            "Id": id, "Name": "/shop-web", "State": {"Running": true, "Pid": 0},
            "Config": {"Image": "shop/web:1", "Labels": {"com.docker.compose.project": "shop", "com.example.owner": "web-team", "com.example.deploy-id": "d-123", "org.example.other": "x"}}
        }))
        .unwrap();
        let c = build(inspect, &cfg.paths.sys.join("fs/cgroup"), &cfg.paths.proc, &cfg.containers);
        let registry = Registry::new();
        registry.inner.write().unwrap().insert(c.id.clone(), Arc::new(c));
        let gpu = gpu::Watcher::with_apps(cfg.interval, vec![gpu::App { pid: 4242, uuid: "1111".into(), memory_used_bytes: 1 << 20 }]);
        let mut m = MetricSet::new();
        let snaps = ContainerCollector::new(&cfg, registry, Some(gpu)).collect(&mut m);
        let _ = std::fs::remove_dir_all(&root);

        let text = m.render(&[]);
        assert!(text.contains(r#"container_label_com_docker_compose_project="shop""#));
        assert!(!text.contains("com_example") && !text.contains("web-team") && !text.contains("d-123"));
        assert!(text.lines().any(|l| l.starts_with("container_accelerator_memory_used_bytes{") && l.ends_with(r#",uuid="1111"} 1048576"#)));
        assert_eq!(snaps[0].meta.labels.keys().collect::<Vec<_>>(), ["com.docker.compose.project", "com.example.deploy-id", "com.example.owner"]);
        assert_eq!(snaps[0].gpus, vec![GpuOut { uuid: "1111".into(), memory_used_bytes: 1 << 20 }]);
    }
}
