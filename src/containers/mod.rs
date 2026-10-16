pub mod cgroup;
pub mod docker;
pub mod layer;
pub mod lifecycle;

use crate::config::{self, Config};
use crate::host::{emit_net, gpu, procfs};
use crate::metrics::{sanitize_label_name, Base, Kind, LabelSet, MetricSet};
use crate::util::{self, log, rate, read};
use docker::Docker;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
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
    /// Docker's `State.Status` at the last inspect.
    #[serde(skip)]
    pub status: String,
    #[serde(skip)]
    pub health: Option<String>,
    #[serde(skip)]
    pub restart_count: u64,
    /// Host path of the overlay upper directory.
    #[serde(skip)]
    pub upper_dir: Option<String>,
}

pub const COMPOSE_PROJECT: &str = "com.docker.compose.project";
pub const COMPOSE_SERVICE: &str = "com.docker.compose.service";

impl Container {
    pub fn compose_project(&self) -> Option<&str> {
        self.compose_project.as_deref()
    }
    pub fn compose_service(&self) -> Option<&str> {
        self.compose_service.as_deref()
    }
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}

/// Running containers, kept current from the Docker API and its event stream.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<String, Arc<Container>>>>,
    life: Arc<Mutex<lifecycle::Tracker>>,
    pub docker_ok: Arc<AtomicBool>,
    synced: Arc<AtomicBool>,
}

impl Registry {
    pub fn new() -> Self {
        Self { inner: Arc::default(), life: Arc::default(), docker_ok: Arc::new(AtomicBool::new(false)), synced: Arc::new(AtomicBool::new(false)) }
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
        spawn_events(docker.clone(), tx.clone(), self.life.clone());
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
                    let c = self.inspected(i, cgroot, proc, rules);
                    next.insert(c.id.clone(), Arc::new(c));
                }
                Err(e) => log::debug!("{e}"),
            }
        }
        for (id, c) in current.into_iter().filter(|(id, _)| !next.contains_key(id)) {
            let st = docker.inspect(&id).ok().map(|i| i.state);
            self.life.lock().unwrap().stopped(c, st.as_ref(), util::now_secs());
        }
        log::debug!("registry: {} running containers", next.len());
        *self.inner.write().unwrap() = next;
        self.synced.store(true, Ordering::Relaxed);
    }

    /// Builds a container from inspect and records its lifecycle state.
    fn inspected(&self, i: docker::Inspect, cgroot: &Path, proc: &Path, rules: &config::Containers) -> Container {
        let mut life = self.life.lock().unwrap();
        life.inspected(&i.id, &i.state);
        if counts(&i.state) {
            life.running(&i.id);
        }
        build(i, cgroot, proc, rules)
    }
}

impl Registry {
    /// Re-inspects only the containers an event named, dropping any that are gone or no longer running.
    fn update(&self, docker: &Docker, cgroot: &Path, proc: &Path, rules: &config::Containers, ids: &[String]) {
        let mut next = self.inner.read().unwrap().clone();
        let now = util::now_secs();
        for id in ids {
            match docker.inspect(id) {
                Ok(i) if counts(&i.state) => {
                    let c = self.inspected(i, cgroot, proc, rules);
                    next.insert(c.id.clone(), Arc::new(c));
                }
                Ok(i) => {
                    let st = i.state.clone();
                    let c = next.remove(id).unwrap_or_else(|| Arc::new(self.inspected(i, cgroot, proc, rules)));
                    self.life.lock().unwrap().stopped(c, Some(&st), now);
                }
                Err(_) => {
                    if let Some(c) = next.remove(id) {
                        self.life.lock().unwrap().stopped(c, None, now);
                    }
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

fn spawn_events(docker: Docker, tx: Sender<Option<String>>, life: Arc<Mutex<lifecycle::Tracker>>) {
    std::thread::Builder::new()
        .name("docker-events".into())
        .spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                let res = docker.events(|ev| {
                    if ev.kind != "container" {
                        return;
                    }
                    life.lock().unwrap().event(&ev.actor.id, &ev.action, &ev.actor.attributes, ev.at(util::now_secs()));
                    if matches!(ev.action.as_str(), "start" | "restart" | "die" | "destroy" | "rename" | "pause" | "unpause") || ev.action.starts_with("health_status") {
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

pub(crate) fn build(i: docker::Inspect, cgroot: &Path, proc: &Path, rules: &config::Containers) -> Container {
    let all_labels = i.config.labels.unwrap_or_default();
    let labels: BTreeMap<String, String> =
        all_labels.iter().filter(|(k, _)| keep_label(&rules.labels, k) || keep_label(&rules.api_labels, k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let name = i.name.trim_start_matches('/').to_string();
    let cgroup = cgroup::find_dir(cgroot, &i.id, proc, i.state.pid);
    let cgroup_path = cgroup
        .as_ref()
        .and_then(|p| p.strip_prefix(cgroot).ok())
        .map(|p| format!("/{}", p.display()))
        .unwrap_or_else(|| format!("/docker/{}", i.id));
    let mut c = Container {
        name,
        image: i.config.image,
        compose_project: all_labels.get(COMPOSE_PROJECT).cloned(),
        compose_service: all_labels.get(COMPOSE_SERVICE).cloned(),
        labels,
        pid: i.state.pid,
        network_mode: i.host_config.network_mode,
        started_at: i.state.started_at,
        created: i.created,
        log_path: i.log_path,
        log_driver: i.host_config.log_config.kind,
        tty: i.config.tty,
        cgroup,
        status: i.state.status,
        health: i.state.health.map(|h| h.status).filter(|h| !h.is_empty()),
        restart_count: i.restart_count,
        upper_dir: i
            .graph_driver
            .data
            .and_then(|mut d| d.remove("UpperDir"))
            .or_else(|| (i.state.pid > 0).then(|| read(proc.join(format!("{}/mountinfo", i.state.pid))).and_then(|t| layer::upper_dir_from_mountinfo(&t))).flatten()),
        id: i.id,
        ..Default::default()
    };
    c.metric_labels = metric_labels(&c, &cgroup_path, &rules.labels);
    c.all_labels = all_labels;
    c
}

/// Identity labels: clean names, and cAdvisor's (`id` is the cgroup path).
fn metric_labels(c: &Container, cgroup_path: &str, keep: &[String]) -> LabelSet {
    let kv = |k: &str, v: &str| (k.to_string(), v.to_string());
    let mut clean = vec![kv("container", &c.name), kv("container_id", c.short_id()), kv("image", &c.image)];
    clean.extend(c.compose_project.as_deref().map(|v| kv("compose_project", v)));
    clean.extend(c.compose_service.as_deref().map(|v| kv("compose_service", v)));
    let mut cadvisor = vec![kv("id", cgroup_path), kv("image", &c.image), kv("name", &c.name)];
    for (k, v) in c.labels.iter().filter(|(k, _)| keep_label(keep, k)) {
        let l = (format!("container_label_{}", sanitize_label_name(k)), v.clone());
        if k != COMPOSE_PROJECT && k != COMPOSE_SERVICE {
            upsert(&mut clean, l.clone());
        }
        upsert(&mut cadvisor, l);
    }
    Arc::new(Base { clean, cadvisor })
}

/// Labels that sanitize to the same name keep the last value.
fn upsert(v: &mut Vec<(String, String)>, (k, val): (String, String)) {
    match v.iter_mut().find(|(n, _)| *n == k) {
        Some(e) => e.1 = val,
        None => v.push((k, val)),
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
    pub lifecycle: Lifecycle,
    pub cpu: CpuOut,
    pub memory: MemOut,
    pub io: Vec<IoOut>,
    pub network: Vec<NetOut>,
    pub pids: PidsOut,
    pub gpus: Vec<GpuOut>,
    /// Null until measured, and always without `containers.writable_layer`.
    pub writable_layer: Option<layer::Size>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Lifecycle {
    pub state: String,
    pub health: Option<String>,
    pub restarts_total: u64,
    pub last_exit: Option<lifecycle::Exit>,
}

/// A container that isn't running, kept for `lifecycle::RETAIN_SECS` after it stopped.
#[derive(Debug, Clone, Serialize)]
pub struct StoppedSnapshot {
    #[serde(flatten)]
    pub meta: Arc<Container>,
    pub lifecycle: Lifecycle,
    /// Unix seconds since the container stopped.
    pub stopped_at: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Collected {
    pub running: Vec<ContainerSnapshot>,
    pub stopped: Vec<StoppedSnapshot>,
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
    pub read_bytes_per_second: Option<f64>,
    pub write_bytes_per_second: Option<f64>,
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
    /// Tasks, counting each thread.
    pub current: u64,
    pub limit: Option<u64>,
    /// Processes; null when `cgroup.procs` can't be read.
    pub processes: Option<u64>,
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
    io: HashMap<String, (u64, u64)>,
}

pub struct ContainerCollector {
    registry: Registry,
    proc: PathBuf,
    sys: PathBuf,
    network: bool,
    prev: HashMap<String, Prev>,
    devices: HashMap<(u32, u32), String>,
    gpu: Option<gpu::Watcher>,
    sizes: layer::Sizes,
}

impl ContainerCollector {
    pub fn new(cfg: &Config, registry: Registry, gpu: Option<gpu::Watcher>, sizes: layer::Sizes) -> Self {
        Self { registry, proc: cfg.paths.proc.clone(), sys: cfg.paths.sys.clone(), network: cfg.containers.network, prev: HashMap::new(), devices: HashMap::new(), gpu, sizes }
    }

    pub fn collect(&mut self, m: &mut MetricSet) -> Collected {
        let now = util::now_secs();
        let mut out = Vec::new();
        let mut next_prev = HashMap::new();
        let ctrs = self.registry.all();
        let ids: HashSet<&str> = ctrs.iter().map(|c| c.id.as_str()).collect();
        let mut gpu_mem = match &self.gpu {
            Some(w) => gpu_by_container(&self.proc, &w.apps(), &ids),
            None => HashMap::new(),
        };
        let (exits, stopped) = {
            let mut life = self.registry.life.lock().unwrap();
            life.expire(now, &ids);
            let exits: HashMap<String, lifecycle::Exit> = ctrs.iter().filter_map(|c| Some((c.id.clone(), life.exit(&c.id)?))).collect();
            let stopped: Vec<_> = life.stopped_list().into_iter().map(|s| (life.exit(&s.meta.id), s)).collect();
            (exits, stopped)
        };
        for c in &ctrs {
            let life = Lifecycle { state: c.status.clone(), health: c.health.clone(), restarts_total: c.restart_count, last_exit: exits.get(&c.id).copied() };
            emit_lifecycle(m, &c.metric_labels, &life);
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
            put(m, "container_memory_rss_bytes", "Size of RSS in bytes.", Kind::Gauge, l, raw.mem_anon as f64);
            put(m, "container_memory_cache_bytes", "Number of bytes of page cache memory.", Kind::Gauge, l, raw.mem_file as f64);
            put(m, "container_memory_mapped_file_bytes", "Size of memory mapped files in bytes.", Kind::Gauge, l, raw.mem_file_mapped as f64);
            put(m, "container_memory_kernel_usage_bytes", "Size of kernel memory allocated in bytes.", Kind::Gauge, l, raw.mem_kernel as f64);
            put(m, "container_memory_total_inactive_file_bytes", "Current total inactive file in bytes.", Kind::Gauge, l, raw.mem_inactive_file as f64);
            put(m, "container_memory_total_active_file_bytes", "Current total active file in bytes.", Kind::Gauge, l, raw.mem_active_file as f64);
            if let Some(p) = raw.mem_peak {
                put(m, "container_memory_max_usage_bytes", "Maximum memory usage recorded in bytes.", Kind::Gauge, l, p as f64);
            }
            if let Some(s) = raw.mem_swap {
                put(m, "container_memory_swap_bytes", "Container swap usage in bytes.", Kind::Gauge, l, s as f64);
            }
            put(m, "container_spec_memory_limit_bytes", "Memory limit for the container.", Kind::Gauge, l, raw.mem_max.unwrap_or(0) as f64);
            put(m, "container_oom_events_total", "Count of out of memory events observed for the container.", Kind::Counter, l, raw.oom_kills as f64);
            for (res, p) in &raw.pressure {
                if let Some(v) = p.some_usec {
                    put(m, &format!("container_pressure_{res}_waiting_seconds_total"), "Total time duration tasks in the container have waited due to this resource.", Kind::Counter, l, v as f64 / 1e6);
                }
                if let Some(v) = p.full_usec {
                    put(m, &format!("container_pressure_{res}_stalled_seconds_total"), "Total time duration no tasks in the container could make progress due to this resource.", Kind::Counter, l, v as f64 / 1e6);
                }
            }

            let mut io = Vec::new();
            let mut io_prev = HashMap::new();
            for s in &raw.io {
                let dev = self.device_name(s.major, s.minor);
                let dl: [(&str, &str); 1] = [("device", &dev)];
                m.push_with("container_fs_reads_bytes_total", "Cumulative count of bytes read.", Kind::Counter, l, &dl, s.rbytes as f64);
                m.push_with("container_fs_writes_bytes_total", "Cumulative count of bytes written.", Kind::Counter, l, &dl, s.wbytes as f64);
                m.push_with("container_fs_reads_total", "Cumulative count of reads completed.", Kind::Counter, l, &dl, s.rios as f64);
                m.push_with("container_fs_writes_total", "Cumulative count of writes completed.", Kind::Counter, l, &dl, s.wios as f64);
                let p = prev.and_then(|p| p.io.get(&dev));
                io.push(IoOut {
                    device: dev.clone(),
                    read_bytes_total: s.rbytes,
                    write_bytes_total: s.wbytes,
                    read_bytes_per_second: p.and_then(|p| rate(p.0, s.rbytes, dt)),
                    write_bytes_per_second: p.and_then(|p| rate(p.1, s.wbytes, dt)),
                });
                io_prev.insert(dev, (s.rbytes, s.wbytes));
            }

            if let Some(n) = raw.procs {
                put(m, "container_processes", "Number of processes running inside the container.", Kind::Gauge, l, n as f64);
            }
            put(m, "container_threads", "Number of threads running inside the container.", Kind::Gauge, l, raw.pids as f64);
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
                m.push_with("container_gpu_memory_used_bytes", "GPU memory held by the container's processes.", Kind::Gauge, l, &[("uuid", &g.uuid)], g.memory_used_bytes as f64);
            }

            let writable_layer = self.sizes.get(&c.id);
            if let Some(w) = writable_layer {
                put(m, "container_writable_layer_bytes", "Bytes in the container's writable layer, measured up to containers.writable_layer_interval ago.", Kind::Gauge, l, w.bytes as f64);
            }

            next_prev.insert(c.id.clone(), Prev { at: now, usage_usec: raw.cpu.usage_usec, net: net_prev, io: io_prev });
            out.push(ContainerSnapshot { meta: c.clone(), lifecycle: life, cpu, memory, io, network, pids: PidsOut { current: raw.pids, limit: raw.pids_max, processes: raw.procs }, gpus, writable_layer });
        }
        self.prev = next_prev;
        let stopped = stopped
            .into_iter()
            .map(|(exit, s)| {
                let life = Lifecycle { state: s.state, health: None, restarts_total: s.meta.restart_count, last_exit: exit };
                emit_lifecycle(m, &s.meta.metric_labels, &life);
                StoppedSnapshot { meta: s.meta, lifecycle: life, stopped_at: s.since }
            })
            .collect();
        Collected { running: out, stopped }
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

fn emit_lifecycle(m: &mut MetricSet, l: &LabelSet, life: &Lifecycle) {
    if !life.state.is_empty() {
        m.push_with("container_state", "Docker state of the container; 1 for the current state.", Kind::Gauge, l, &[("state", &life.state)], 1.0);
    }
    if let Some(h) = &life.health {
        m.push_with("container_health", "Docker health check status of the container; 1 for the current status.", Kind::Gauge, l, &[("health", h)], 1.0);
    }
    put(m, "container_restarts_total", "Times Docker restarted the container under its restart policy.", Kind::Counter, l, life.restarts_total as f64);
    if let Some(e) = life.last_exit {
        put(m, "container_last_exit_code", "Exit code of the container's last exit.", Kind::Gauge, l, e.code as f64);
        put(m, "container_oom_killed", "Whether Docker reported an OOM kill before the container's last exit.", Kind::Gauge, l, if e.oom_killed { 1.0 } else { 0.0 });
    }
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
    fn labels_sanitizing_alike_appear_once() {
        let labels = BTreeMap::from([("com.x.a-b".to_string(), "dash".to_string()), ("com.x.a_b".to_string(), "under".to_string())]);
        let c = Container { id: "d".repeat(64), name: "web".into(), labels, ..Default::default() };
        let l = metric_labels(&c, "/docker/d", &["com.x.*".to_string()]);
        for set in [&l.clean, &l.cadvisor] {
            let got: Vec<_> = set.iter().filter(|(k, _)| k == "container_label_com_x_a_b").collect();
            assert_eq!(got, [&("container_label_com_x_a_b".to_string(), "under".to_string())]);
        }
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
        std::fs::write(cg.join("pids.current"), "68\n").unwrap();
        std::fs::write(cg.join("cgroup.procs"), "4242\n4243\n4244\n4245\n").unwrap();
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
        let sizes = layer::Sizes::default();
        sizes.set(&id, layer::Size { bytes: 4096, measured_at: 1.5 });
        let mut m = MetricSet::new();
        let snaps = ContainerCollector::new(&cfg, registry, Some(gpu), sizes).collect(&mut m).running;
        let _ = std::fs::remove_dir_all(&root);

        let text = m.render(&[], config::Compat::None);
        assert!(text.contains(r#"{container="shop-web",container_id="cccccccccccc",image="shop/web:1",compose_project="shop"}"#));
        assert!(!text.contains("container_label_com_docker_compose_project"));
        assert!(!text.contains("container_pressure_"), "no *.pressure files in the fixture");
        assert!(text.lines().any(|l| l.starts_with("container_processes{") && l.ends_with("} 4")));
        assert!(text.lines().any(|l| l.starts_with("container_threads{") && l.ends_with("} 68")));
        assert!(!text.contains("com_example") && !text.contains("web-team") && !text.contains("d-123"));
        assert!(text.lines().any(|l| l.starts_with("container_gpu_memory_used_bytes{") && l.ends_with(r#",uuid="1111"} 1048576"#)));
        let old = m.render(&[], config::Compat::Cadvisor);
        let id = format!("/system.slice/docker-{id}.scope");
        assert!(old.contains(&format!(r#"container_cpu_usage_seconds_total{{id="{id}",image="shop/web:1",name="shop-web",container_label_com_docker_compose_project="shop",cpu="total"}} 0.001"#)));
        assert!(old.contains("container_gpu_memory_used_bytes{") && !old.contains("accelerator"));
        assert!(!old.contains("com_example") && !old.contains("container_id="));
        assert_eq!(snaps[0].meta.labels.keys().collect::<Vec<_>>(), ["com.docker.compose.project", "com.example.deploy-id", "com.example.owner"]);
        assert_eq!(snaps[0].gpus, vec![GpuOut { uuid: "1111".into(), memory_used_bytes: 1 << 20 }]);
        assert!(text.contains(r#"container_writable_layer_bytes{container="shop-web",container_id="cccccccccccc",image="shop/web:1",compose_project="shop"} 4096"#));
        assert_eq!(serde_json::to_value(&snaps[0]).unwrap()["pids"], serde_json::json!({"current": 68, "limit": null, "processes": 4}));
        assert_eq!(serde_json::to_value(&snaps[0]).unwrap()["writable_layer"], serde_json::json!({"bytes": 4096, "measured_at": 1.5}));
    }

    #[test]
    fn lifecycle_series_for_running_and_stopped_containers() {
        let root = std::env::temp_dir().join(format!("collector-life-{}", std::process::id()));
        let cfg = Config::default();
        let inspect = |id: &str, name: &str, state: serde_json::Value| -> docker::Inspect {
            serde_json::from_value(serde_json::json!({"Id": id, "Name": name, "RestartCount": 2, "State": state, "Config": {"Image": "app:1"}})).unwrap()
        };
        let (web, job) = ("a".repeat(64), "b".repeat(64));
        let registry = Registry::new();
        let none = BTreeMap::new();
        {
            let mut life = registry.life.lock().unwrap();
            life.event(&web, "oom", &none, 100.0);
            life.event(&web, "die", &BTreeMap::from([("exitCode".to_string(), "137".to_string())]), 100.5);
            life.event(&web, "start", &none, 101.0);
        }
        let c = registry.inspected(inspect(&web, "/web", serde_json::json!({"Status": "running", "Running": true, "Pid": 0, "Health": {"Status": "healthy"}})), &root, &root, &cfg.containers);
        registry.inner.write().unwrap().insert(web.clone(), Arc::new(c));
        let gone = registry.inspected(inspect(&job, "/job", serde_json::json!({"Status": "running", "Running": true, "Pid": 0})), &root, &root, &cfg.containers);
        let finished = util::now_secs() - 30.0;
        let st = docker::InspectState { status: "exited".into(), exit_code: 3, finished_at: format_rfc3339(finished), ..Default::default() };
        registry.life.lock().unwrap().stopped(Arc::new(gone), Some(&st), util::now_secs());

        let mut m = MetricSet::new();
        let got = ContainerCollector::new(&cfg, registry, None, layer::Sizes::default()).collect(&mut m);
        let text = m.render(&[], config::Compat::None);
        let has = |line: &str| text.lines().any(|l| l == line);
        let web_l = format!(r#"container="web",container_id="{}",image="app:1""#, &web[..12]);
        let job_l = format!(r#"container="job",container_id="{}",image="app:1""#, &job[..12]);
        assert!(has(&format!(r#"container_state{{{web_l},state="running"}} 1"#)));
        assert!(has(&format!(r#"container_health{{{web_l},health="healthy"}} 1"#)));
        assert!(has(&format!("container_restarts_total{{{web_l}}} 2")));
        assert!(has(&format!("container_last_exit_code{{{web_l}}} 137")));
        assert!(has(&format!("container_oom_killed{{{web_l}}} 1")));
        assert!(has(&format!(r#"container_state{{{job_l},state="exited"}} 1"#)));
        assert!(has(&format!("container_last_exit_code{{{job_l}}} 3")));
        assert!(has(&format!("container_oom_killed{{{job_l}}} 0")));
        assert!(!text.contains(&format!(r#"container_health{{{job_l}"#)));
        // No cgroup, so no resource readings for either.
        assert!(got.running.is_empty() && !text.contains("container_cpu_usage_seconds_total"));
        assert_eq!(got.stopped.len(), 1);
        assert_eq!(got.stopped[0].lifecycle.last_exit.map(|e| e.code), Some(3));
        assert!((got.stopped[0].stopped_at - finished).abs() < 1.0);
    }

    fn format_rfc3339(t: f64) -> String {
        let (days, secs) = ((t as i64).div_euclid(86400), (t as i64).rem_euclid(86400));
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
    }

    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    }
}
