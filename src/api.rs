use crate::config::Compat;
use crate::containers::{ContainerSnapshot, StoppedSnapshot};
use crate::host::gpu::Gpu;
use crate::host::HostSnapshot;
use crate::logs::Stats as LogStats;
use crate::metrics::{MetricSet, TextReader};
use crate::state::{Shared, Snapshot};
use crate::util::{self, log};
use serde::Serialize;
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

/// Concurrent `/api/v1/stream` clients; each holds a thread.
const MAX_STREAMS: usize = 16;

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
    /// Binds `listen` and serves on `workers` threads; returns the bound address.
    pub fn start(self, listen: &str, workers: usize) -> Result<Option<std::net::SocketAddr>, String> {
        let server = Arc::new(Server::http(listen).map_err(|e| format!("listen {listen}: {e}"))?);
        let addr = server.server_addr().to_ip();
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
        Ok(addr)
    }

    fn authorized(&self, req: &Request) -> bool {
        if self.token.is_empty() {
            return true;
        }
        let want = format!("Bearer {}", self.token);
        req.headers().iter().any(|h| h.field.equiv("Authorization") && constant_eq(h.value.as_str().as_bytes(), want.as_bytes()))
    }

    fn handle(self: &Arc<Self>, req: Request) {
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
        let view = match View::parse(&query) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json(400, &Error { error: e }));
                return;
            }
        };
        if path == "/api/v1/stream" {
            self.stream(req, view);
            return;
        }
        let res = match path.as_str() {
            "/api/v1/host" => {
                let snap = self.shared.read();
                match &snap.host {
                    Some(h) => json(200, &HostView { host: h, gpus: &snap.gpus }),
                    None => json(503, &Error { error: "no data yet" }),
                }
            }
            "/api/v1/containers" => {
                let snap = self.shared.read();
                json(200, &self.items(&snap, view))
            }
            "/api/v1/gpus" => {
                let snap = self.shared.read();
                json(200, &gpu_views(&snap))
            }
            "/api/v1/snapshot" => {
                let snap = self.shared.read();
                if snap.collected_at > 0.0 {
                    json(200, &self.snapshot(&snap, view))
                } else {
                    json(503, &Error { error: "no data yet" })
                }
            }
            p if p.starts_with("/api/v1/containers/") => {
                let key = &p["/api/v1/containers/".len()..];
                let snap = self.shared.read();
                match find(&snap.containers, key) {
                    Ok(c) => json(200, &self.item(c, snap.collected_at)),
                    Err(e) => json(404, &Error { error: e }),
                }
            }
            "" => text(200, "collector: /metrics /health /api/v1/snapshot /api/v1/stream /api/v1/host /api/v1/containers /api/v1/containers/:id /api/v1/gpus\n", "text/plain; charset=utf-8"),
            _ => json(404, &Error { error: "not found" }),
        };
        let _ = req.respond(res);
    }

    fn items<'a>(&self, snap: &'a Snapshot, view: View) -> Vec<ContainerItem<'a>> {
        view.apply(&snap.containers).into_iter().map(|c| self.item(c, snap.collected_at)).collect()
    }

    fn snapshot<'a>(&self, snap: &'a Snapshot, view: View) -> SnapshotView<'a> {
        SnapshotView {
            host: snap.host.as_ref(),
            gpus: gpu_views(snap),
            gpu_per_container: snap.gpu_per_container.unwrap_or(false),
            containers: self.items(snap, view),
            stopped: &snap.stopped,
            collected_at: millis(snap.collected_at),
            interval_seconds: self.interval.as_secs_f64(),
        }
    }

    /// Server-sent events: the latest snapshot now, then each new one. Collection runs at `live_interval` while any client is connected.
    fn stream(self: &Arc<Self>, req: Request, view: View) {
        let Some(sub) = self.shared.live.subscribe(MAX_STREAMS) else {
            let _ = req.respond(json(503, &Error { error: "too many streams" }));
            return;
        };
        let api = self.clone();
        let spawned = std::thread::Builder::new().name("stream".into()).spawn(move || {
            let _sub = sub;
            let mut w = req.into_writer();
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
                return;
            }
            let mut seen = api.shared.live.seq();
            let mut next = Some(seen);
            while !util::shutdown::requested() {
                let chunk = match next {
                    Some(seq) => {
                        seen = seq;
                        let snap = api.shared.read();
                        if snap.collected_at > 0.0 {
                            let mut b = b"event: snapshot\ndata: ".to_vec();
                            let _ = serde_json::to_writer(&mut b, &api.snapshot(&snap, view));
                            b.extend_from_slice(b"\n\n");
                            b
                        } else {
                            Vec::new()
                        }
                    }
                    None => b": keepalive\n\n".to_vec(),
                };
                if !chunk.is_empty() && w.write_all(&chunk).and_then(|_| w.flush()).is_err() {
                    return;
                }
                next = api.shared.live.wait(seen, Duration::from_secs(15));
            }
        });
        if let Err(e) = spawned {
            log::warn!("http: stream: {e}");
        }
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
            gpu_per_container: (!snap.gpus.is_empty()).then(|| snap.gpu_per_container.unwrap_or(false)),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu_per_container: Option<bool>,
}

#[derive(Serialize)]
struct HostView<'a> {
    #[serde(flatten)]
    host: &'a HostSnapshot,
    gpus: &'a [Gpu],
}

#[derive(Serialize)]
struct SnapshotView<'a> {
    host: Option<&'a HostSnapshot>,
    gpus: Vec<GpuView<'a>>,
    gpu_per_container: bool,
    containers: Vec<ContainerItem<'a>>,
    stopped: &'a [StoppedSnapshot],
    collected_at: f64,
    interval_seconds: f64,
}

/// A GPU with the containers holding memory on it; `containers` is null when per-container GPU memory isn't read.
#[derive(Serialize)]
struct GpuView<'a> {
    #[serde(flatten)]
    gpu: &'a Gpu,
    containers: Option<Vec<GpuUser<'a>>>,
}

#[derive(Serialize)]
struct GpuUser<'a> {
    id: &'a str,
    name: &'a str,
    memory_used_bytes: u64,
}

fn gpu_views(snap: &Snapshot) -> Vec<GpuView<'_>> {
    let per_container = snap.gpu_per_container.unwrap_or(false);
    snap.gpus
        .iter()
        .map(|g| GpuView {
            gpu: g,
            containers: per_container.then(|| {
                let mut users: Vec<GpuUser> = snap
                    .containers
                    .iter()
                    .flat_map(|c| c.gpus.iter().filter(|u| u.uuid == g.uuid).map(move |u| GpuUser { id: &c.meta.id, name: &c.meta.name, memory_used_bytes: u.memory_used_bytes }))
                    .collect();
                users.sort_by(|a, b| b.memory_used_bytes.cmp(&a.memory_used_bytes).then_with(|| a.name.cmp(b.name)));
                users
            }),
        })
        .collect()
}

/// Sum of rates; unknown if any is unknown.
fn sum(mut it: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    it.try_fold(0.0, |acc, v| v.map(|v| acc + v))
}

/// Looks a container up by full id, name or a unique id prefix of four or more characters.
fn find<'a>(containers: &'a [ContainerSnapshot], key: &str) -> Result<&'a ContainerSnapshot, &'static str> {
    if let Some(c) = containers.iter().find(|c| c.meta.id == key || c.meta.name == key) {
        return Ok(c);
    }
    let mut hits = containers.iter().filter(|c| key.len() >= 4 && c.meta.id.starts_with(key));
    match (hits.next(), hits.next()) {
        (Some(c), None) => Ok(c),
        (Some(_), Some(_)) => Err("ambiguous id prefix"),
        _ => Err("container not found"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SortKey {
    Name,
    Cpu,
    Memory,
    NetRx,
    NetTx,
    DiskRead,
    DiskWrite,
}

/// `sort` and `limit` for container lists.
#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    sort: SortKey,
    limit: Option<usize>,
}

impl View {
    fn parse(q: &[(String, String)]) -> Result<View, &'static str> {
        let sort = match param(q, "sort").unwrap_or("name") {
            "name" => SortKey::Name,
            "cpu" => SortKey::Cpu,
            "memory" => SortKey::Memory,
            "net_rx" => SortKey::NetRx,
            "net_tx" => SortKey::NetTx,
            "disk_read" => SortKey::DiskRead,
            "disk_write" => SortKey::DiskWrite,
            _ => return Err("sort must be cpu, memory, net_rx, net_tx, disk_read, disk_write or name"),
        };
        let limit = match param(q, "limit") {
            None | Some("") => None,
            Some(v) => Some(v.parse().map_err(|_| "limit must be a non-negative integer")?),
        };
        Ok(View { sort, limit })
    }

    /// Sorted descending by the key (ascending for name), unknown values last, ties by name; then truncated.
    fn apply<'a>(&self, containers: &'a [ContainerSnapshot]) -> Vec<&'a ContainerSnapshot> {
        let mut v: Vec<(&ContainerSnapshot, Option<f64>)> = containers.iter().map(|c| (c, self.value(c))).collect();
        v.sort_by(|(a, va), (b, vb)| {
            let by_value = match (va, vb) {
                (Some(x), Some(y)) => y.partial_cmp(x).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            by_value.then_with(|| a.meta.name.cmp(&b.meta.name))
        });
        v.truncate(self.limit.unwrap_or(usize::MAX));
        v.into_iter().map(|(c, _)| c).collect()
    }

    fn value(&self, c: &ContainerSnapshot) -> Option<f64> {
        match self.sort {
            SortKey::Name => None,
            SortKey::Cpu => c.cpu.usage_cores,
            SortKey::Memory => Some(c.memory.working_set_bytes as f64),
            SortKey::NetRx if c.meta.network_mode != "host" => sum(c.network.iter().map(|n| n.receive_bytes_per_second)),
            SortKey::NetTx if c.meta.network_mode != "host" => sum(c.network.iter().map(|n| n.transmit_bytes_per_second)),
            SortKey::NetRx | SortKey::NetTx => None,
            SortKey::DiskRead => sum(c.io.iter().map(|d| d.read_bytes_per_second)),
            SortKey::DiskWrite => sum(c.io.iter().map(|d| d.write_bytes_per_second)),
        }
    }
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

    fn ctr(name: &str, cores: Option<f64>, ws: u64, rx: &[Option<f64>], read: &[Option<f64>]) -> ContainerSnapshot {
        use crate::containers::{Container, CpuOut, IoOut, MemOut, NetOut};
        ContainerSnapshot {
            meta: Arc::new(Container { id: format!("{name:0<64}"), name: name.into(), network_mode: "bridge".into(), ..Default::default() }),
            cpu: CpuOut { usage_cores: cores, ..Default::default() },
            memory: MemOut { working_set_bytes: ws, ..Default::default() },
            network: rx.iter().map(|r| NetOut { receive_bytes_per_second: *r, transmit_bytes_per_second: Some(1.0), ..Default::default() }).collect(),
            io: read.iter().map(|r| IoOut { read_bytes_per_second: *r, write_bytes_per_second: Some(0.0), ..Default::default() }).collect(),
            ..Default::default()
        }
    }

    fn names(v: &[&ContainerSnapshot]) -> Vec<String> {
        v.iter().map(|c| c.meta.name.clone()).collect()
    }

    fn view(q: &str) -> Result<View, &'static str> {
        View::parse(&parse_query(&format!("/x?{q}")))
    }

    #[test]
    fn sorts_and_limits_containers() {
        let cs = vec![
            ctr("db", Some(0.5), 900, &[Some(10.0), Some(5.0)], &[Some(100.0)]),
            ctr("web", Some(1.5), 300, &[Some(40.0)], &[Some(1.0), Some(2.0)]),
            ctr("cache", Some(0.5), 600, &[None], &[]),
            ctr("new", None, 100, &[], &[None]),
        ];
        assert_eq!(names(&view("").unwrap().apply(&cs)), ["cache", "db", "new", "web"]);
        // Ties (db and cache at 0.5) go by name; no reading yet sorts last.
        assert_eq!(names(&view("sort=cpu").unwrap().apply(&cs)), ["web", "cache", "db", "new"]);
        assert_eq!(names(&view("sort=memory&limit=2").unwrap().apply(&cs)), ["db", "cache"]);
        // Sums over interfaces and devices; an interface without a rate makes the sum unknown.
        assert_eq!(names(&view("sort=net_rx").unwrap().apply(&cs)), ["web", "db", "new", "cache"]);
        assert_eq!(names(&view("sort=disk_read").unwrap().apply(&cs)), ["db", "web", "cache", "new"]);
        assert_eq!(view("sort=name&limit=0").unwrap().apply(&cs).len(), 0);
        assert!(view("sort=pids").is_err());
        assert!(view("limit=-1").is_err());
    }

    #[test]
    fn host_network_containers_sort_last_on_network() {
        let mut host = ctr("agent", Some(0.1), 1, &[], &[]);
        Arc::get_mut(&mut host.meta).unwrap().network_mode = "host".into();
        let cs = vec![host, ctr("web", Some(0.1), 1, &[Some(0.0)], &[])];
        assert_eq!(names(&view("sort=net_tx").unwrap().apply(&cs)), ["web", "agent"]);
    }

    #[test]
    fn finds_containers_by_unique_prefix() {
        let cs = vec![ctr("abc1", None, 0, &[], &[]), ctr("abc2", None, 0, &[], &[])];
        assert_eq!(find(&cs, "abc2").unwrap().meta.name, "abc2");
        assert_eq!(find(&cs, "abc1000").unwrap().meta.name, "abc1");
        assert_eq!(find(&cs, "abc").err(), Some("container not found"));
        assert_eq!(find(&cs, "abc0").err(), Some("container not found"));
        let cs = vec![ctr("abcd1", None, 0, &[], &[]), ctr("abcd2", None, 0, &[], &[])];
        assert_eq!(find(&cs, "abcd").err(), Some("ambiguous id prefix"));
    }

    #[test]
    fn gpu_views_list_containers_only_when_known() {
        use crate::containers::GpuOut;
        let gpu = |uuid: &str| Gpu { index: 0, uuid: uuid.into(), name: "GPU".into(), temperature_celsius: None, utilization_ratio: None, memory_utilization_ratio: None, memory_used_bytes: None, memory_total_bytes: None, power_draw_watts: None, fan_speed_ratio: None };
        let mut a = ctr("a", None, 0, &[], &[]);
        a.gpus = vec![GpuOut { uuid: "1111".into(), memory_used_bytes: 10 }];
        let mut b = ctr("b", None, 0, &[], &[]);
        b.gpus = vec![GpuOut { uuid: "1111".into(), memory_used_bytes: 30 }, GpuOut { uuid: "2222".into(), memory_used_bytes: 5 }];
        let mut snap = Snapshot { gpus: vec![gpu("1111"), gpu("2222"), gpu("3333")], containers: vec![a, b], gpu_per_container: Some(true), ..Default::default() };
        let v = serde_json::to_value(gpu_views(&snap)).unwrap();
        assert_eq!(v[0]["containers"], serde_json::json!([{"id": format!("{:0<64}", "b"), "name": "b", "memory_used_bytes": 30}, {"id": format!("{:0<64}", "a"), "name": "a", "memory_used_bytes": 10}]));
        assert_eq!(v[1]["containers"].as_array().unwrap().len(), 1);
        assert_eq!(v[2]["containers"], serde_json::json!([]));
        snap.gpu_per_container = Some(false);
        assert!(serde_json::to_value(gpu_views(&snap)).unwrap()[0]["containers"].is_null());
    }

    fn serve(shared: Shared, token: &str) -> std::net::SocketAddr {
        let api = Api { token: token.into(), shared, log_stats: None, interval: Duration::from_secs(15), docker_ok: Arc::default(), containers_enabled: false };
        api.start("127.0.0.1:0", 2).unwrap().unwrap()
    }

    fn get(addr: std::net::SocketAddr, path: &str, token: &str) -> (u16, String) {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let code = out[9..12].parse().unwrap();
        (code, out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
    }

    #[test]
    fn stream_sends_each_snapshot_and_counts_subscribers() {
        use std::io::{BufRead, BufReader};
        let shared = Shared::default();
        shared.store(Snapshot { collected_at: 100.0, containers: vec![ctr("a", Some(0.1), 1, &[], &[]), ctr("b", Some(0.9), 1, &[], &[])], ..Default::default() });
        let addr = serve(shared.clone(), "t");
        let s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(&s, "GET /api/v1/stream?sort=cpu&limit=1 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\r\n").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut event = || {
            let mut line = String::new();
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if let Some(d) = line.strip_prefix("data: ") {
                    return serde_json::from_str::<serde_json::Value>(d).unwrap();
                }
            }
        };
        let first = event();
        assert_eq!(first["collected_at"], 100.0);
        assert_eq!(first["containers"].as_array().unwrap().len(), 1);
        assert_eq!(first["containers"][0]["name"], "b");
        assert_eq!(shared.live.subscribers(), 1);
        shared.store(Snapshot { collected_at: 105.0, ..Default::default() });
        assert_eq!(event()["collected_at"], 105.0);
        drop(r);
        let _ = s.shutdown(std::net::Shutdown::Both);
        // The stream notices the closed socket on its next write.
        for t in 0..50 {
            shared.store(Snapshot { collected_at: 110.0 + t as f64, ..Default::default() });
            if shared.live.subscribers() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(shared.live.subscribers(), 0);
    }

    #[test]
    fn routes_answer_with_json_and_errors() {
        let shared = Shared::default();
        let addr = serve(shared.clone(), "t");
        assert_eq!(get(addr, "/api/v1/snapshot", "t").0, 503);
        assert_eq!(get(addr, "/api/v1/containers", "wrong").0, 401);
        assert_eq!(get(addr, "/health", "wrong").0, 503);
        shared.store(Snapshot { collected_at: util::now_secs(), containers: vec![ctr("web", None, 1, &[], &[])], ..Default::default() });
        let (code, body) = get(addr, "/api/v1/snapshot?sort=memory", "t");
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["containers"][0]["name"], "web");
        assert!(v["host"].is_null() && v["gpus"].as_array().unwrap().is_empty() && v["stopped"].as_array().unwrap().is_empty());
        assert_eq!(get(addr, "/api/v1/containers?sort=bogus", "t").0, 400);
        assert_eq!(get(addr, "/metrics?compat=prometheus", "t").0, 400);
        assert_eq!(get(addr, "/api/v1/gpus", "t"), (200, "[]".to_string()));
        assert_eq!(get(addr, "/health", "").0, 200);
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
