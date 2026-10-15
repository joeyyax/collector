use crate::config::Compat;
use crate::containers::ContainerSnapshot;
use crate::logs::Stats as LogStats;
use crate::metrics::{MetricSet, TextReader};
use std::io::Read;
use crate::state::Shared;
use crate::util::{self, log};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

pub struct Api {
    pub token: String,
    pub shared: Shared,
    pub log_stats: Option<Arc<LogStats>>,
    pub interval: Duration,
    pub docker_ok: Arc<std::sync::atomic::AtomicBool>,
    pub containers_enabled: bool,
}

use crate::VERSION;

impl Api {
    /// Binds `listen` and serves on `workers` threads.
    pub fn start(self, listen: &str, workers: usize) -> Result<(), String> {
        let server = Arc::new(Server::http(listen).map_err(|e| format!("listen {listen}: {e}"))?);
        log::info!("listening on {listen} ({} workers, auth {})", workers.max(1), if self.token.is_empty() { "off" } else { "on" });
        let api = Arc::new(self);
        for i in 0..workers.max(1) {
            let (server, api) = (server.clone(), api.clone());
            std::thread::Builder::new()
                .name(format!("http-{i}"))
                .spawn(move || loop {
                    match server.recv() {
                        Ok(req) => api.handle(req),
                        Err(e) => log::warn!("http: {e}"),
                    }
                })
                .map_err(|e| format!("spawn http worker: {e}"))?;
        }
        Ok(())
    }

    fn authorized(&self, req: &Request) -> bool {
        if self.token.is_empty() {
            return true;
        }
        let want = format!("Bearer {}", self.token);
        req.headers().iter().any(|h| h.field.equiv("Authorization") && constant_eq(h.value.as_str().as_bytes(), want.as_bytes()))
    }

    fn handle(&self, req: Request) {
        if *req.method() != Method::Get && *req.method() != Method::Head {
            let _ = req.respond(Response::from_string("method not allowed\n").with_status_code(405));
            return;
        }
        let path = req.url().split('?').next().unwrap_or("/").trim_end_matches('/').to_string();
        if path == "/health" || path == "/healthz" {
            let _ = req.respond(self.health());
            return;
        }
        if !self.authorized(&req) {
            let res = json(401, &Error { error: "unauthorized" }).with_header(Header::from_bytes("WWW-Authenticate", "Bearer").unwrap());
            let _ = req.respond(res);
            return;
        }
        let query = parse_query(req.url());
        if path == "/metrics" {
            let Some(mode) = Compat::parse(param(&query, "compat").unwrap_or("")) else {
                let _ = req.respond(json(400, &Error { error: "compat must be cadvisor or none" }));
                return;
            };
            let gzip = req.headers().iter().any(|h| h.field.equiv("Accept-Encoding") && h.value.as_str().contains("gzip"));
            let body = self.metrics_body(mode);
            let ctype = Header::from_bytes("Content-Type", "text/plain; version=0.0.4; charset=utf-8").unwrap();
            let _ = if gzip {
                let enc = flate2::read::GzEncoder::new(body, flate2::Compression::fast());
                req.respond(Response::new(200.into(), vec![ctype, Header::from_bytes("Content-Encoding", "gzip").unwrap()], enc, None, None))
            } else {
                req.respond(Response::new(200.into(), vec![ctype], body, None, None))
            };
            return;
        }
        let res = match path.as_str() {
            "/api/v1/host" => match &self.shared.read().host {
                Some(h) => json(200, h),
                None => json(503, &Error { error: "no data yet" }),
            },
            "/api/v1/containers" => {
                let snap = self.shared.read();
                json(200, &snap.containers.iter().map(|c| self.item(c, snap.collected_at)).collect::<Vec<_>>())
            }
            p if p.starts_with("/api/v1/containers/") => {
                let key = &p["/api/v1/containers/".len()..];
                let snap = self.shared.read();
                let found = snap
                    .containers
                    .iter()
                    .find(|c| c.meta.id == key || c.meta.name == key)
                    .or_else(|| if key.len() >= 4 { snap.containers.iter().find(|c| c.meta.id.starts_with(key)) } else { None });
                match found {
                    Some(c) => json(200, &self.item(c, snap.collected_at)),
                    None => json(404, &Error { error: "container not found" }),
                }
            }
            "" => text(200, "collector: /metrics /health /api/v1/host /api/v1/containers /api/v1/containers/:id\n", "text/plain; charset=utf-8"),
            _ => json(404, &Error { error: "not found" }),
        };
        let _ = req.respond(res);
    }

    fn item<'a>(&self, c: &'a ContainerSnapshot, collected_at: f64) -> ContainerItem<'a> {
        ContainerItem { c, collected_at: millis(collected_at), interval_seconds: self.interval.as_secs_f64() }
    }

    /// Streams the last collection, then the collector's own metrics.
    fn metrics_body(&self, mode: Compat) -> impl Read + Send {
        let set = self.shared.read().metrics.clone();
        let mut own = MetricSet::new();
        self.process_metrics(&mut own);
        TextReader::new(set, mode).chain(std::io::Cursor::new(own.render(&[], mode).into_bytes()))
    }

    fn process_metrics(&self, m: &mut MetricSet) {
        if let Some((ut, st, rss)) = util::read("/proc/self/stat").and_then(|s| crate::host::procfs::parse_pid_stat(&s)) {
            // SAFETY: sysconf has no preconditions.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as f64;
            m.counter("process_cpu_seconds_total", "Total user and system CPU time spent in seconds.", &[], (ut + st) as f64 / util::clk_tck());
            m.gauge("process_resident_memory_bytes", "Resident memory size in bytes.", &[], rss as f64 * page);
        }
        m.gauge("collector_heap_bytes", "Bytes currently allocated on the heap.", &[], crate::alloc::live_bytes() as f64);
        m.gauge("collector_build_info", "Collector version.", &[("version", VERSION)], 1.0);
        let snap = self.shared.read();
        m.gauge("collector_collect_duration_seconds", "Time the last collection took.", &[], snap.collect_seconds);
        m.gauge("collector_last_collect_timestamp_seconds", "Time of the last collection.", &[], snap.collected_at);
        m.gauge("collector_series", "Series produced by the last collection.", &[], snap.metrics.len() as f64);
        m.gauge("collector_containers", "Containers measured in the last collection.", &[], snap.containers.len() as f64);
        drop(snap);
        if self.containers_enabled {
            m.gauge("collector_docker_up", "Whether the Docker API answered the last sync.", &[], if self.docker_ok.load(std::sync::atomic::Ordering::Relaxed) { 1.0 } else { 0.0 });
        }
        if let Some(s) = &self.log_stats {
            s.emit(m);
        }
        self.shared.remote_emit(m);
    }

    fn health(&self) -> Response<std::io::Cursor<Vec<u8>>> {
        let snap = self.shared.read();
        let now = util::now_secs();
        let age = now - snap.collected_at;
        let fresh = fresh(snap.collected_at, now, self.interval);
        let body = Health {
            status: if fresh { "ok" } else { "stale" },
            version: VERSION,
            host: snap.host.as_ref().map(|h| h.host.clone()).unwrap_or_default(),
            last_collect_age_seconds: (snap.collected_at > 0.0).then(|| millis(age)),
            collected_at: (snap.collected_at > 0.0).then(|| millis(snap.collected_at)),
            interval_seconds: self.interval.as_secs_f64(),
            docker: self.containers_enabled.then(|| self.docker_ok.load(std::sync::atomic::Ordering::Relaxed)),
            logs_last_success: self.log_stats.as_ref().map(|s| s.last_success.load(std::sync::atomic::Ordering::Relaxed)),
        };
        json(if fresh { 200 } else { 503 }, &body)
    }
}

#[derive(Serialize)]
struct Error {
    error: &'static str,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    host: String,
    last_collect_age_seconds: Option<f64>,
    collected_at: Option<f64>,
    interval_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    docker: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs_last_success: Option<u64>,
}

/// A container with the time and interval of the collection it came from.
#[derive(Serialize)]
struct ContainerItem<'a> {
    #[serde(flatten)]
    c: &'a ContainerSnapshot,
    collected_at: f64,
    interval_seconds: f64,
}

/// A collection is fresh until three intervals plus 5 s have passed.
fn fresh(collected_at: f64, now: f64, interval: Duration) -> bool {
    collected_at > 0.0 && now - collected_at < interval.as_secs_f64() * 3.0 + STALE_SLACK
}

const STALE_SLACK: f64 = 5.0;

/// Query parameters in order, percent-decoded.
fn parse_query(url: &str) -> Vec<(String, String)> {
    let Some((_, q)) = url.split_once('?') else { return Vec::new() };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
            (b'+', ..) => out.push(b' '),
            (c, ..) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn millis(secs: f64) -> f64 {
    (secs * 1000.0).round() / 1000.0
}

fn text(code: u16, body: &str, ctype: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body.as_bytes().to_vec())
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", ctype).unwrap())
}

fn json<T: Serialize + ?Sized>(code: u16, v: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_tokens() {
        assert!(constant_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_eq(b"Bearer abd", b"Bearer abc"));
        assert!(!constant_eq(b"Bearer ab", b"Bearer abc"));
    }

    #[test]
    fn parses_query_strings() {
        let q = parse_query("/api/v1/containers?sort=net%5Frx&limit=5&x&compat=cadvisor&limit=3");
        assert_eq!(param(&q, "sort"), Some("net_rx"));
        assert_eq!(param(&q, "limit"), Some("3"));
        assert_eq!(param(&q, "x"), Some(""));
        assert_eq!(param(&q, "nope"), None);
        assert!(parse_query("/metrics").is_empty());
        assert_eq!(decode("a%2"), "a%2");
        assert_eq!(decode("%e2%9c%93+%zz"), "\u{2713} %zz");
    }

    #[test]
    fn stale_after_three_intervals_plus_slack() {
        let i = Duration::from_secs(15);
        assert!(!fresh(0.0, 100.0, i));
        assert!(fresh(1000.0, 1049.9, i));
        assert!(!fresh(1000.0, 1050.0, i));
        // A 1 s interval still tolerates one slow collection.
        assert!(fresh(1000.0, 1004.0, Duration::from_secs(1)));
        assert!(!fresh(1000.0, 1008.0, Duration::from_secs(1)));
    }

    #[test]
    fn container_items_carry_freshness() {
        let c = ContainerSnapshot { meta: Arc::new(crate::containers::Container { name: "shop-web".into(), ..Default::default() }), ..Default::default() };
        let v = serde_json::to_value(ContainerItem { c: &c, collected_at: millis(1791656309.73149), interval_seconds: 60.0 }).unwrap();
        assert_eq!(v["name"], "shop-web");
        assert_eq!(v["collected_at"], 1791656309.731);
        assert_eq!(v["interval_seconds"], 60.0);
        assert!(v["gpus"].as_array().unwrap().is_empty());
    }
}
