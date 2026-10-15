mod alloc;
mod api;
mod config;
mod containers;
mod host;
mod logs;
mod metrics;
mod remote_write;
mod state;
mod util;

use config::{Config, Toggle};
use host::gpu;
use metrics::MetricSet;
use state::{Shared, Snapshot};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use util::log;

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// Release version: COLLECTOR_VERSION at build time, else the crate version. The Dockerfile sets it empty when no VERSION is given.
pub const VERSION: &str = match option_env!("COLLECTOR_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

const USAGE: &str = "usage: collector [--config FILE] [--once] [--version]

  --config FILE  TOML config (default: $COLLECTOR_CONFIG, else built-in defaults)
  --once         collect once, print /metrics output and exit
  --version      print the version

Every setting can be overridden with COLLECTOR_<KEY> or COLLECTOR_<SECTION>__<KEY>.";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path = std::env::var("COLLECTOR_CONFIG").ok();
    let mut once = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => config_path = args.next(),
            "--once" => once = true,
            "--version" | "-V" => {
                println!("collector {VERSION}");
                return;
            }
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(if a == "--help" || a == "-h" { 0 } else { 2 });
            }
        }
    }
    let cfg = match Config::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("collector: {e}");
            std::process::exit(2);
        }
    };
    util::log::set_level(&cfg.log_level);

    let registry = containers::Registry::new();
    if cfg.containers.enabled || cfg.logs.enabled {
        registry.start(&cfg);
    }
    let gpu = (cfg.gpu.enabled != Toggle::Off && (cfg.host_metrics.enabled || cfg.containers.enabled)).then(|| {
        let per_process = match (cfg.containers.enabled, gpu::host_pid_namespace(&cfg.paths.proc)) {
            (false, _) => gpu::PerProcess::Off,
            (true, false) => gpu::PerProcess::NoHostPid,
            (true, true) => gpu::PerProcess::On,
        };
        gpu::Watcher::start(&cfg.gpu.nvidia_smi, cfg.interval, cfg.gpu.enabled == Toggle::Auto, per_process)
    });
    let mut host = cfg.host_metrics.enabled.then(|| host::HostCollector::new(&cfg, gpu.clone()));
    let mut ctrs = cfg.containers.enabled.then(|| containers::ContainerCollector::new(&cfg, registry.clone(), gpu));
    let shared = Shared::default();

    if once {
        let deadline = Instant::now() + Duration::from_secs(3);
        while cfg.containers.enabled && !registry.synced() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let s = collect(host.as_mut(), ctrs.as_mut());
        print!("{}", s.metrics.render(&[], config::Compat::None));
        return;
    }

    // Every thread inherits this mask, so only the main thread's sigwait sees SIGTERM and SIGINT.
    let signals = signals::block();

    let log_stats = cfg.logs.enabled.then(|| Arc::new(logs::Stats::default()));
    if let Some(stats) = &log_stats {
        logs::Shipper::new(&cfg, registry.clone(), stats.clone()).start();
    }

    {
        let shared = shared.clone();
        let cfg = cfg.clone();
        let synced = registry.clone();
        std::thread::Builder::new()
            .name("collect".into())
            .spawn(move || {
                let wait = Instant::now() + Duration::from_secs(3);
                while cfg.containers.enabled && !synced.synced() && Instant::now() < wait {
                    std::thread::sleep(Duration::from_millis(50));
                }
                while !util::shutdown::requested() {
                    let started = Instant::now();
                    shared.store(collect(host.as_mut(), ctrs.as_mut()));
                    util::shutdown::sleep(cfg.interval.saturating_sub(started.elapsed()));
                }
            })
            .expect("spawn collect");
    }

    let remote = !cfg.remote_write.url.is_empty();
    if remote {
        let (shared, rw, mode, timeout) = (shared.clone(), cfg.remote_write.clone(), cfg.metrics.compat, cfg.shutdown_timeout);
        std::thread::Builder::new().name("remote-write".into()).spawn(move || remote_write::run(rw, mode, shared, timeout)).expect("spawn remote-write");
    }

    log::info!(
        "collector {} host={} interval={}s containers={} logs={} remote_write={}",
        VERSION,
        cfg.host,
        cfg.interval.as_secs_f64(),
        cfg.containers.enabled,
        cfg.logs.enabled,
        !cfg.remote_write.url.is_empty()
    );
    let api = api::Api {
        token: cfg.api.token.clone(),
        shared: shared.clone(),
        log_stats: log_stats.clone(),
        interval: cfg.interval,
        docker_ok: registry.docker_ok.clone(),
        containers_enabled: cfg.containers.enabled,
    };
    if let Err(e) = api.start(&cfg.listen, cfg.api.workers) {
        log::error!("{e}");
        std::process::exit(1);
    }

    let sig = signals::wait(&signals);
    log::info!("received signal {sig}; flushing");
    util::shutdown::request();
    let deadline = Instant::now() + cfg.shutdown_timeout + Duration::from_secs(2);
    let done = || log_stats.as_ref().is_none_or(|s| s.stopped.load(Ordering::SeqCst)) && (!remote || shared.remote.stopped.load(Ordering::SeqCst));
    while !done() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    log::info!("exiting");
    std::process::exit(0);
}

mod signals {
    pub struct Set(libc::sigset_t);

    /// Blocks SIGTERM and SIGINT in this thread and every thread spawned after.
    pub fn block() -> Set {
        // SAFETY: sigset_t is plain data, initialized by sigemptyset before use.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            Set(set)
        }
    }

    /// Waits for one of the blocked signals.
    pub fn wait(set: &Set) -> i32 {
        let mut sig = 0;
        // SAFETY: set was filled by block(); sig is a valid out pointer.
        unsafe { libc::sigwait(&set.0, &mut sig) };
        sig
    }
}

fn collect(host: Option<&mut host::HostCollector>, ctrs: Option<&mut containers::ContainerCollector>) -> Snapshot {
    let started = Instant::now();
    let mut m = MetricSet::new();
    let host = host.map(|h| h.collect(&mut m));
    let containers = ctrs.map(|c| c.collect(&mut m)).unwrap_or_default();
    Snapshot {
        host,
        containers,
        metrics: Arc::new(m),
        collected_at: util::now_secs(),
        collect_seconds: started.elapsed().as_secs_f64(),
    }
}
