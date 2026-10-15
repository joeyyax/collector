use crate::util::log;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
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

/// Keeps one `nvidia-smi -lms` process running and holds its latest readings.
#[derive(Clone)]
pub struct Watcher {
    latest: Arc<Mutex<BTreeMap<u32, (Gpu, Instant)>>>,
    max_age: Duration,
}

impl Watcher {
    pub fn start(nvidia_smi: &str, interval: Duration, auto: bool) -> Self {
        let w = Self { latest: Arc::default(), max_age: interval * 3 };
        if cfg!(not(feature = "gpu")) {
            log::info!("gpu metrics off: built without the gpu feature");
            return w;
        }
        let me = w.clone();
        let bin = nvidia_smi.to_string();
        std::thread::Builder::new().name("gpu".into()).spawn(move || me.run(&bin, interval, auto)).expect("spawn gpu");
        w
    }

    fn run(&self, bin: &str, interval: Duration, auto: bool) {
        let mut backoff = Duration::from_secs(5);
        loop {
            let started = Instant::now();
            let mut lines = 0usize;
            let res = Command::new(bin)
                .args([format!("--query-gpu={QUERY}"), "--format=csv,noheader,nounits".into(), format!("--loop-ms={}", interval.as_millis())])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            match res {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            for g in parse(&line) {
                                lines += 1;
                                self.latest.lock().unwrap().insert(g.index, (g, Instant::now()));
                            }
                        }
                    }
                    let _ = child.wait();
                }
                Err(e) => {
                    if auto {
                        log::info!("gpu metrics off: {bin}: {e}");
                        return;
                    }
                    log::warn!("gpu: {bin}: {e}");
                }
            }
            if lines == 0 && auto {
                log::info!("gpu metrics off: {bin} reported no GPUs");
                return;
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(5);
            }
            log::warn!("gpu: {bin} exited; restarting in {}s", backoff.as_secs());
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(300));
        }
    }

    /// Readings newer than three intervals, by GPU index.
    pub fn latest(&self) -> Vec<Gpu> {
        let map = self.latest.lock().unwrap();
        map.values().filter(|(_, at)| at.elapsed() < self.max_age).map(|(g, _)| g.clone()).collect()
    }
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
}
