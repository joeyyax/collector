use crate::util::log;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Gpu {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub temperature_celsius: Option<f64>,
    pub utilization_ratio: Option<f64>,
    pub memory_utilization_ratio: Option<f64>,
    pub memory_used_bytes: Option<f64>,
    pub memory_total_bytes: Option<f64>,
    pub power_draw_watts: Option<f64>,
    pub fan_speed_ratio: Option<f64>,
}

pub const QUERY: &str = "index,uuid,name,temperature.gpu,utilization.gpu,utilization.memory,memory.used,memory.total,power.draw,fan.speed";
pub const APPS_QUERY: &str = "pid,gpu_uuid,used_memory";

/// One process with a context on a GPU, from `--query-compute-apps`.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub pid: u32,
    pub uuid: String,
    pub memory_used_bytes: u64,
}

/// Whether to follow per-process GPU memory alongside the GPU readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PerProcess {
    Off,
    /// Wanted, but nvidia-smi can't see other containers' processes from this PID namespace.
    NoHostPid,
    On,
}

/// The newest `--query-compute-apps` round. Rounds have no delimiter, so a line more than `gap` after a round began starts the next one.
#[derive(Default)]
struct Rounds {
    apps: Vec<App>,
    began: Option<Instant>,
    last: Option<Instant>,
}

impl Rounds {
    fn feed(&mut self, app: App, now: Instant, gap: Duration) {
        if self.began.is_none_or(|b| now.duration_since(b) > gap) {
            self.apps.clear();
            self.began = Some(now);
        }
        self.apps.push(app);
        self.last = Some(now);
    }

    /// An idle GPU prints nothing, so a round older than `max_age` means no processes.
    fn current(&self, now: Instant, max_age: Duration) -> Vec<App> {
        if self.last.is_some_and(|l| now.duration_since(l) < max_age) {
            self.apps.clone()
        } else {
            Vec::new()
        }
    }
}

/// Keeps `nvidia-smi --loop-ms` processes running and holds their latest readings: one for GPUs and, once a GPU shows up, one for GPU processes.
#[derive(Clone)]
pub struct Watcher {
    latest: Arc<Mutex<BTreeMap<u32, (Gpu, Instant)>>>,
    apps: Arc<Mutex<Rounds>>,
    per_container: Arc<AtomicBool>,
    interval: Duration,
}

impl Watcher {
    pub fn start(nvidia_smi: &str, interval: Duration, auto: bool, per_process: PerProcess) -> Self {
        let w = Self { latest: Arc::default(), apps: Arc::default(), per_container: Arc::default(), interval };
        if cfg!(not(feature = "gpu")) {
            log::info!("gpu metrics off: built without the gpu feature");
            return w;
        }
        let me = w.clone();
        let bin = nvidia_smi.to_string();
        std::thread::Builder::new().name("gpu".into()).spawn(move || me.run(&bin, auto, per_process)).expect("spawn gpu");
        w
    }

    fn run(&self, bin: &str, auto: bool, per_process: PerProcess) {
        let mut backoff = Duration::from_secs(5);
        // Auto mode gives up only if nvidia-smi never reported a GPU.
        let mut seen = false;
        loop {
            let started = Instant::now();
            match spawn(bin, &[format!("--query-gpu={QUERY}"), "--format=csv,noheader,nounits".into(), format!("--loop-ms={}", self.interval.as_millis())]) {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            for g in parse(&line) {
                                if !seen {
                                    seen = true;
                                    self.start_apps(bin, per_process);
                                }
                                self.latest.lock().unwrap().insert(g.index, (g, Instant::now()));
                            }
                        }
                    }
                    let _ = child.wait();
                }
                Err(e) => {
                    if auto && !seen {
                        log::info!("gpu metrics off: {bin}: {e}");
                        return;
                    }
                    log::warn!("gpu: {bin}: {e}");
                }
            }
            if !seen && auto {
                log::info!("gpu metrics off: {bin} reported no GPUs");
                return;
            }
            wait_restart(bin, started, &mut backoff);
        }
    }

    fn start_apps(&self, bin: &str, per_process: PerProcess) {
        match per_process {
            PerProcess::Off => {}
            PerProcess::NoHostPid => log::info!("gpu: per-container memory off: nvidia-smi lists only processes in its own PID namespace; run the collector with pid: host"),
            PerProcess::On => {
                self.per_container.store(true, Ordering::Relaxed);
                let (me, bin) = (self.clone(), bin.to_string());
                std::thread::Builder::new().name("gpu-apps".into()).spawn(move || me.run_apps(&bin)).expect("spawn gpu-apps");
            }
        }
    }

    fn run_apps(&self, bin: &str) {
        let gap = self.interval / 2;
        let mut backoff = Duration::from_secs(5);
        loop {
            let started = Instant::now();
            match spawn(bin, &[format!("--query-compute-apps={APPS_QUERY}"), "--format=csv,noheader,nounits".into(), format!("--loop-ms={}", self.interval.as_millis())]) {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            if let Some(a) = parse_app(&line) {
                                self.apps.lock().unwrap().feed(a, Instant::now(), gap);
                            }
                        }
                    }
                    let _ = child.wait();
                }
                Err(e) => log::warn!("gpu: {bin}: {e}"),
            }
            wait_restart(bin, started, &mut backoff);
        }
    }

    #[cfg(test)]
    pub fn with_apps(interval: Duration, apps: Vec<App>) -> Self {
        let w = Self { latest: Arc::default(), apps: Arc::default(), per_container: Arc::new(AtomicBool::new(true)), interval };
        let now = Instant::now();
        apps.into_iter().for_each(|a| w.apps.lock().unwrap().feed(a, now, interval));
        w
    }

    /// GPU processes from the newest round; empty when no process has a GPU context.
    pub fn apps(&self) -> Vec<App> {
        self.apps.lock().unwrap().current(Instant::now(), self.interval * 3 / 2)
    }

    /// Whether per-process GPU memory is being read, so a container's empty `gpus` means it holds no GPU memory.
    pub fn per_container(&self) -> bool {
        self.per_container.load(Ordering::Relaxed)
    }

    /// Readings newer than three intervals, by GPU index.
    pub fn latest(&self) -> Vec<Gpu> {
        let map = self.latest.lock().unwrap();
        map.values().filter(|(_, at)| at.elapsed() < self.interval * 3).map(|(g, _)| g.clone()).collect()
    }
}

/// Starts nvidia-smi with stdout piped; it gets SIGTERM when the thread that started it exits.
fn spawn(bin: &str, args: &[String]) -> std::io::Result<Child> {
    let mut cmd = Command::new(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe and touches no parent memory.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    cmd.spawn()
}

fn wait_restart(bin: &str, started: Instant, backoff: &mut Duration) {
    if started.elapsed() > Duration::from_secs(60) {
        *backoff = Duration::from_secs(5);
    }
    log::warn!("gpu: {bin} exited; restarting in {}s", backoff.as_secs());
    std::thread::sleep(*backoff);
    *backoff = (*backoff * 2).min(Duration::from_secs(300));
}

/// True when `proc` is the procfs of this process's own PID namespace, so GPU process ids resolve there.
pub fn host_pid_namespace(proc: &Path) -> bool {
    std::fs::read_link(proc.join("self")).ok().and_then(|p| p.to_str()?.parse::<u32>().ok()) == Some(std::process::id())
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse().ok()
}

pub fn parse(csv: &str) -> Vec<Gpu> {
    let mib = 1024.0 * 1024.0;
    csv.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 10 {
                return None;
            }
            Some(Gpu {
                index: f[0].parse().ok()?,
                uuid: f[1].trim_start_matches("GPU-").to_string(),
                name: f[2].to_string(),
                temperature_celsius: num(f[3]),
                utilization_ratio: num(f[4]).map(|v| v / 100.0),
                memory_utilization_ratio: num(f[5]).map(|v| v / 100.0),
                memory_used_bytes: num(f[6]).map(|v| v * mib),
                memory_total_bytes: num(f[7]).map(|v| v * mib),
                power_draw_watts: num(f[8]),
                fan_speed_ratio: num(f[9]).map(|v| v / 100.0),
            })
        })
        .collect()
}

/// Parses one `--query-compute-apps` line: pid, GPU uuid, MiB used.
pub fn parse_app(line: &str) -> Option<App> {
    let mut f = line.split(',').map(str::trim);
    let pid = f.next()?.parse().ok()?;
    let uuid = f.next()?.trim_start_matches("GPU-").to_string();
    let mib: u64 = f.next()?.parse().ok()?;
    Some(App { pid, uuid, memory_used_bytes: mib << 20 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv() {
        let g = parse("0, GPU-11111111-2222-3333-4444-555555555555, NVIDIA GeForce RTX 3060, 36, 1, 0, 20, 12288, 14.09, 30\n1, GPU-abc, Tesla T4, 41, [N/A], [N/A], 1925, 15360, [N/A], [N/A]\n");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].uuid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(g[0].temperature_celsius, Some(36.0));
        assert_eq!(g[0].utilization_ratio, Some(0.01));
        assert_eq!(g[0].memory_used_bytes, Some(20.0 * 1024.0 * 1024.0));
        assert_eq!(g[0].power_draw_watts, Some(14.09));
        assert_eq!(g[1].power_draw_watts, None);
        assert_eq!(g[1].utilization_ratio, None);
    }

    #[test]
    fn parses_compute_apps() {
        let csv = "948830, GPU-11111111-2222-3333-4444-555555555555, 1582\n1438842, GPU-66666666-7777-8888-9999-000000000000, 374\n4242, GPU-abc, [N/A]\nNo running processes found\n";
        let apps: Vec<App> = csv.lines().filter_map(parse_app).collect();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0], App { pid: 948830, uuid: "11111111-2222-3333-4444-555555555555".into(), memory_used_bytes: 1582 << 20 });
        assert_eq!(apps[1].pid, 1438842);
    }

    #[test]
    fn rounds_replace_and_expire() {
        let (t0, s) = (Instant::now(), Duration::from_secs(1));
        let gap = Duration::from_millis(7500);
        let app = |pid| App { pid, uuid: "u".into(), memory_used_bytes: 1 };
        let mut r = Rounds::default();
        r.feed(app(1), t0, gap);
        r.feed(app(2), t0 + Duration::from_millis(2), gap);
        assert_eq!(r.current(t0 + s, 22 * s).len(), 2);
        // The next round drops pid 1.
        r.feed(app(2), t0 + 15 * s, gap);
        assert_eq!(r.current(t0 + 16 * s, 22 * s), vec![app(2)]);
        // No lines for a round and a half: nothing has a context.
        assert!(r.current(t0 + 38 * s, 22 * s).is_empty());
    }
}
