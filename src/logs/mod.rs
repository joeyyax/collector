pub mod assemble;
pub mod cursor;
pub mod jsonfile;
pub mod sink;

use crate::config::{Config, StartAt};
use crate::containers::docker::Docker;
use crate::containers::{rfc3339_nanos, Container, Registry};
use crate::metrics::MetricSet;
use crate::util::{self, log, shutdown};
use assemble::{Assembler, Piece, Record, Rules};
use cursor::{ApiSaved, Cursor, Saved, Start, State};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Stats {
    pub lines: AtomicU64,
    pub bytes: AtomicU64,
    pub batches: AtomicU64,
    pub errors: AtomicU64,
    pub unparsed: AtomicU64,
    pub lost_rotations: AtomicU64,
    pub followed: AtomicU64,
    pub followed_api: AtomicU64,
    pub unsupported: AtomicU64,
    pub last_success: AtomicU64,
    /// Set once the shipper has flushed and saved its cursors after a shutdown request.
    pub stopped: AtomicBool,
}

impl Stats {
    pub fn emit(&self, m: &mut MetricSet) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
        m.counter("collector_log_lines_shipped_total", "Log records delivered to the sink.", &[], g(&self.lines));
        m.counter("collector_log_bytes_shipped_total", "Uncompressed request bytes delivered to the sink.", &[], g(&self.bytes));
        m.counter("collector_log_batches_total", "Log batches delivered.", &[], g(&self.batches));
        m.counter("collector_log_send_errors_total", "Failed log batch sends; each is retried.", &[], g(&self.errors));
        m.counter("collector_log_lines_unparsed_total", "Log file lines that were not valid json-file entries.", &[], g(&self.unparsed));
        m.counter("collector_log_rotations_missed_total", "Times a cursor's file rotated away before it was read.", &[], g(&self.lost_rotations));
        m.gauge("collector_log_containers_followed", "Containers whose log files are followed.", &[], g(&self.followed));
        m.gauge("collector_log_containers_followed_api", "Containers followed through the Docker logs API (local, journald).", &[], g(&self.followed_api));
        m.gauge("collector_log_containers_unsupported", "Running containers whose log driver can't be read.", &[], g(&self.unsupported));
        m.gauge("collector_log_last_success_timestamp_seconds", "Time of the last delivered batch.", &[], g(&self.last_success));
    }
}

/// One message from a Docker logs API follower.
struct ApiLine {
    id: String,
    stream: &'static str,
    time: String,
    ns: u64,
    text: String,
}

enum Source {
    File { path: PathBuf, read: Cursor, replay: Option<Cursor>, asm: Assembler<Cursor> },
    Api { last_ns: u64, replay_ns: Option<u64>, asm: Assembler<u64>, stop: Arc<AtomicBool> },
}

struct Tracked {
    meta: Arc<Container>,
    source: Source,
    gone: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Commit {
    File(Saved),
    Api(ApiSaved),
}

enum Driver {
    File,
    Api,
    Unsupported,
}

pub struct Shipper {
    cfg: crate::config::Logs,
    host: String,
    containers_dir: PathBuf,
    state_path: PathBuf,
    registry: Registry,
    docker: Docker,
    stats: Arc<Stats>,
    exclude: Vec<regex_lite::Regex>,
    exclude_label: Option<(String, String)>,
    self_id: Option<String>,
    rules: Rules,
    shutdown_timeout: Duration,
}

impl Shipper {
    pub fn new(cfg: &Config, registry: Registry, stats: Arc<Stats>) -> Self {
        let exclude = cfg.logs.exclude_names.iter().filter_map(|p| util::regex(p)).collect();
        let exclude_label = cfg.logs.exclude_label.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()));
        Self {
            cfg: cfg.logs.clone(),
            host: cfg.host.clone(),
            containers_dir: cfg.paths.docker_containers.clone(),
            state_path: cfg.paths.state_dir.join("log-cursors.json"),
            registry,
            docker: Docker::new(&cfg.paths.docker_socket),
            stats,
            exclude,
            exclude_label,
            self_id: own_container_id(),
            rules: Rules::new(&cfg.logs.multiline),
            shutdown_timeout: cfg.shutdown_timeout,
        }
    }

    pub fn start(self) {
        std::thread::Builder::new().name("logs".into()).spawn(move || self.run()).expect("spawn logs");
    }

    fn excluded(&self, c: &Container) -> bool {
        self.self_id.as_deref().is_some_and(|id| c.id.starts_with(id))
            || self.exclude.iter().any(|r| r.is_match(&c.name))
            || self.exclude_label.as_ref().is_some_and(|(k, v)| c.all_labels.get(k) == Some(v))
    }

    fn driver(c: &Container) -> Driver {
        match c.log_driver.as_str() {
            "json-file" | "" => Driver::File,
            "local" | "journald" => Driver::Api,
            _ => Driver::Unsupported,
        }
    }

    fn log_file(&self, c: &Container) -> PathBuf {
        let p = Path::new(&c.log_path);
        match (p.parent().and_then(|d| d.file_name()), p.file_name()) {
            (Some(dir), Some(file)) => self.containers_dir.join(dir).join(file),
            _ => self.containers_dir.join(&c.id).join(format!("{}-json.log", c.id)),
        }
    }

    fn run(self) {
        let mut sink = match sink::build(&self.cfg) {
            Ok(s) => s,
            Err(e) => {
                log::error!("logs disabled: {e}");
                self.stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(self.state_path.parent().unwrap()) {
            log::error!("logs disabled: state dir: {e}");
            self.stats.stopped.store(true, Ordering::SeqCst);
            return;
        }
        let loaded = State::load(&self.state_path);
        let first_run = loaded.is_none();
        let mut state = loaded.unwrap_or(State { version: 2, ..Default::default() });
        state.version = 2;
        while !self.registry.synced() {
            if shutdown::sleep(Duration::from_millis(200)) {
                self.stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
        }
        let (tx, rx) = sync_channel::<ApiLine>(self.cfg.api_queue_lines.max(16));
        let mut tracked: HashMap<String, Tracked> = HashMap::new();
        let mut warned: HashSet<String> = HashSet::new();
        let mut first = true;
        let mut rr = 0usize;
        let mut last_prune = Instant::now();
        loop {
            let stopping = shutdown::requested();
            if !stopping {
                self.refresh(&mut tracked, &mut state, &mut warned, &tx, first && first_run);
                if first {
                    let _ = state.save(&self.state_path);
                    first = false;
                }
            }
            let ok = self.drain(&mut *sink, &mut tracked, &mut state, &rx, &mut rr, stopping);
            let ok = ok && (stopping || !shutdown::requested() || self.drain(&mut *sink, &mut tracked, &mut state, &rx, &mut rr, true));
            if stopping || shutdown::requested() {
                if !ok {
                    log::warn!("logs: undelivered lines will be re-read on restart");
                }
                for t in tracked.values() {
                    if let Source::Api { stop, .. } = &t.source {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                if let Err(e) = state.save(&self.state_path) {
                    log::warn!("logs: saving cursors: {e}");
                }
                log::info!("logs: stopped, cursors saved");
                self.stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
            for t in tracked.values().filter(|t| t.gone) {
                if let Source::Api { stop, .. } = &t.source {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            tracked.retain(|_, t| !t.gone);

            if last_prune.elapsed() > Duration::from_secs(600) {
                state.cursors.retain(|id, _| tracked.contains_key(id) || self.containers_dir.join(id).exists());
                state.api.retain(|id, _| tracked.contains_key(id));
                let _ = state.save(&self.state_path);
                last_prune = Instant::now();
            }
            shutdown::sleep(self.cfg.poll);
        }
    }

    /// Updates the follow set from the registry; containers that left get one final drain.
    fn refresh(&self, tracked: &mut HashMap<String, Tracked>, state: &mut State, warned: &mut HashSet<String>, tx: &SyncSender<ApiLine>, initial: bool) {
        for t in tracked.values_mut() {
            t.gone = true;
        }
        let mut unsupported = 0;
        for c in self.registry.all() {
            if self.excluded(&c) {
                continue;
            }
            if let Some(t) = tracked.get_mut(&c.id) {
                t.gone = false;
                t.meta = c;
                continue;
            }
            let skip_history = initial && self.cfg.first_run == StartAt::End;
            let source = match Self::driver(&c) {
                Driver::File => {
                    let path = self.log_file(&c);
                    let saved = match state.cursors.get(&c.id) {
                        Some(s) => *s,
                        None => {
                            let start = if skip_history { Start::End } else { Start::Beginning };
                            let Some(cur) = cursor::initial(&path, start) else { continue };
                            let s = Saved::at(cur);
                            state.cursors.insert(c.id.clone(), s);
                            s
                        }
                    };
                    let replay = saved.replay_until.filter(|r| cursor::reachable(&path, *r));
                    Source::File { path, read: saved.cursor(), replay, asm: Assembler::default() }
                }
                Driver::Api => {
                    let saved = match state.api.get(&c.id) {
                        Some(s) => *s,
                        None => {
                            let since_ns = if skip_history { util::now_nanos() } else { 0 };
                            let s = ApiSaved { since_ns, replay_until_ns: None };
                            state.api.insert(c.id.clone(), s);
                            s
                        }
                    };
                    let stop = Arc::new(AtomicBool::new(false));
                    self.spawn_follower(&c, saved.since_ns, stop.clone(), tx.clone());
                    Source::Api { last_ns: saved.since_ns.saturating_sub(1), replay_ns: saved.replay_until_ns, asm: Assembler::default(), stop }
                }
                Driver::Unsupported => {
                    unsupported += 1;
                    if warned.insert(c.id.clone()) {
                        log::warn!("logs: {} uses the {} log driver, which can't be read back; its logs are not shipped", c.name, c.log_driver);
                    }
                    continue;
                }
            };
            tracked.insert(c.id.clone(), Tracked { meta: c, source, gone: false });
        }
        let live = tracked.values().filter(|t| !t.gone);
        let (mut files, mut api) = (0, 0);
        for t in live {
            match t.source {
                Source::File { .. } => files += 1,
                Source::Api { .. } => api += 1,
            }
        }
        self.stats.followed.store(files, Ordering::Relaxed);
        self.stats.followed_api.store(api, Ordering::Relaxed);
        self.stats.unsupported.store(unsupported, Ordering::Relaxed);
    }

    fn spawn_follower(&self, c: &Container, since_ns: u64, stop: Arc<AtomicBool>, tx: SyncSender<ApiLine>) {
        let docker = self.docker.clone();
        let (id, name, tty) = (c.id.clone(), c.name.clone(), c.tty);
        let spawned = std::thread::Builder::new().name(format!("logs-api-{}", &id[..12.min(id.len())])).spawn(move || {
            let mut since = since_ns;
            let mut backoff = Duration::from_secs(1);
            while !stop.load(Ordering::Relaxed) {
                let res = docker.follow_logs(&id, since, tty, &stop, |stream, payload| {
                    let text = String::from_utf8_lossy(payload);
                    let (time, body) = text.split_once(' ').unwrap_or((&text, ""));
                    let Some(ns) = rfc3339_nanos(time) else { return true };
                    since = ns + 1;
                    let line = ApiLine { id: id.clone(), stream, time: time.to_string(), ns, text: body.to_string() };
                    tx.send(line).is_ok() && !stop.load(Ordering::Relaxed)
                });
                if let Err(e) = res {
                    log::debug!("logs: {name}: {e}");
                }
                if shutdown::sleep(backoff) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
        if let Err(e) = spawned {
            log::warn!("logs: {}: {e}", c.name);
        }
    }

    /// Ships batches until every source is caught up. When stopping, flushes groups and stops reading.
    /// Returns false when a batch was abandoned at shutdown; in-memory positions are then ahead of what was delivered.
    fn drain(&self, sink: &mut dyn sink::Sink, tracked: &mut HashMap<String, Tracked>, state: &mut State, rx: &Receiver<ApiLine>, rr: &mut usize, stopping: bool) -> bool {
        let mut ids: Vec<String> = tracked.keys().cloned().collect();
        ids.sort();
        let deadline = Instant::now() + self.shutdown_timeout;
        loop {
            let mut batch = Vec::with_capacity(64 << 10);
            let mut lines = 0usize;
            let mut full = false;
            let mut recs = Vec::new();
            let mut api_recs = Vec::new();
            let limit = |b: &Vec<u8>, l: usize| b.len() >= self.cfg.batch_bytes || l >= self.cfg.batch_lines;

            if !stopping {
                while !full {
                    let Ok(l) = rx.try_recv() else { break };
                    let Some(t) = tracked.get_mut(&l.id) else { continue };
                    if let Source::Api { last_ns, replay_ns, asm, .. } = &mut t.source {
                        if l.ns <= *last_ns {
                            continue;
                        }
                        let replaying = replay_ns.is_some_and(|r| l.ns <= r);
                        asm.push(&self.rules, Piece { stream: l.stream, time: &l.time, text: &l.text, start: l.ns, end: l.ns }, &mut api_recs);
                        *last_ns = l.ns;
                        if replaying {
                            api_recs.clear();
                        } else {
                            *replay_ns = None;
                        }
                        for r in api_recs.drain(..) {
                            self.append(&mut batch, &mut lines, &t.meta, &r.stream, &r.time, &r.msg);
                        }
                        full = limit(&batch, lines);
                    }
                }

                let n = ids.len();
                for k in 0..n {
                    if full {
                        break;
                    }
                    let id = &ids[(*rr + k) % n];
                    let t = tracked.get_mut(id).unwrap();
                    let Source::File { path, read, replay, asm } = &mut t.source else { continue };
                    let Some(chunk) = cursor::read_from(path, *read, self.cfg.read_bytes) else { continue };
                    if chunk.lost {
                        self.stats.lost_rotations.fetch_add(1, Ordering::Relaxed);
                        *replay = None;
                        log::warn!("logs: {} rotated past its cursor; some lines were not shipped", t.meta.name);
                    }
                    for l in &chunk.lines {
                        let Some(parsed) = jsonfile::parse_line(chunk.line(l)) else {
                            self.stats.unparsed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        asm.push(&self.rules, Piece { stream: &parsed.stream, time: &parsed.time, text: &parsed.log, start: l.start, end: l.end }, &mut recs);
                        if let Some(r) = *replay {
                            recs.clear();
                            if l.end.inode == r.inode && l.end.offset >= r.offset {
                                *replay = None;
                            }
                        }
                        for r in recs.drain(..) {
                            self.append(&mut batch, &mut lines, &t.meta, &r.stream, &r.time, &r.msg);
                        }
                    }
                    *read = chunk.cursor;
                    if limit(&batch, lines) {
                        full = true;
                        *rr = (*rr + k + 1) % n;
                    }
                }
            }

            // Idle multi-line groups, gone containers and shutdown flushes.
            for t in tracked.values_mut() {
                let mut out: Vec<Record<()>> = Vec::new();
                match &mut t.source {
                    Source::File { replay: None, asm, .. } => collect(asm, &self.rules, t.gone, stopping, &mut out),
                    Source::Api { replay_ns: None, asm, .. } => collect(asm, &self.rules, t.gone, stopping, &mut out),
                    _ => {}
                }
                for r in out {
                    self.append(&mut batch, &mut lines, &t.meta, &r.stream, &r.time, &r.msg);
                }
            }

            let commits: Vec<(String, Commit)> = tracked.iter().filter_map(|(id, t)| commit_for(t).map(|c| (id.clone(), c))).filter(|(id, c)| !same(state, id, c)).collect();
            let delivered = batch.is_empty() || self.send(sink, &batch, lines, stopping.then_some(deadline));
            if !delivered {
                return false;
            }
            if !commits.is_empty() {
                for (id, c) in commits {
                    match c {
                        Commit::File(s) => {
                            state.cursors.insert(id, s);
                        }
                        Commit::Api(s) => {
                            state.api.insert(id, s);
                        }
                    }
                }
                if let Err(e) = state.save(&self.state_path) {
                    log::warn!("logs: saving cursors: {e}");
                }
            }
            if !full || stopping {
                return true;
            }
        }
    }

    /// Retries until the sink accepts the batch; the log files are the buffer. Gives up only past `deadline`.
    fn send(&self, sink: &mut dyn sink::Sink, batch: &[u8], lines: usize, mut deadline: Option<Instant>) -> bool {
        let mut backoff = Duration::from_millis(500);
        loop {
            match sink.send(batch) {
                Ok(()) => {
                    self.stats.lines.fetch_add(lines as u64, Ordering::Relaxed);
                    self.stats.bytes.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    self.stats.batches.fetch_add(1, Ordering::Relaxed);
                    self.stats.last_success.store(util::now_secs() as u64, Ordering::Relaxed);
                    return true;
                }
                Err(e) => {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    if deadline.is_some_and(|d| Instant::now() + backoff > d) {
                        log::warn!("logs: {e}; giving up for shutdown, unsent lines stay on disk");
                        return false;
                    }
                    log::warn!("logs: {e}; retrying in {:.1}s", backoff.as_secs_f64());
                    if shutdown::sleep(backoff) && deadline.is_none() {
                        deadline = Some(Instant::now() + self.shutdown_timeout);
                    }
                    backoff = (backoff * 2).min(self.cfg.max_backoff);
                }
            }
        }
    }

    fn append(&self, batch: &mut Vec<u8>, lines: &mut usize, c: &Container, stream: &str, time: &str, msg: &str) {
        if let Some(rec) = self.encode(c, stream, time, msg) {
            batch.extend_from_slice(&rec);
            batch.push(b'\n');
            *lines += 1;
        }
    }

    fn encode(&self, c: &Container, stream: &str, time: &str, msg: &str) -> Option<Vec<u8>> {
        let mut rec = Map::new();
        rec.insert("_time".into(), Value::String(time.to_string()));
        rec.insert("_msg".into(), Value::String(msg.to_string()));
        rec.insert("stream".into(), Value::String(stream.to_string()));
        rec.insert("host".into(), Value::String(self.host.clone()));
        rec.insert("container".into(), Value::String(c.name.clone()));
        rec.insert("container_id".into(), Value::String(c.short_id().to_string()));
        rec.insert("image".into(), Value::String(c.image.clone()));
        if let Some(p) = c.compose_project() {
            rec.insert("compose_project".into(), Value::String(p.to_string()));
        }
        if let Some(s) = c.compose_service() {
            rec.insert("compose_service".into(), Value::String(s.to_string()));
        }
        for (k, v) in c.labels.iter().filter(|(k, _)| !k.starts_with("com.docker.compose.")) {
            rec.entry(k.clone()).or_insert_with(|| Value::String(v.clone()));
        }
        if (self.cfg.parse_level || !self.cfg.promote_fields.is_empty()) && msg.trim_start().starts_with('{') {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(msg) {
                if self.cfg.parse_level {
                    if let Some(l) = ["level", "lvl", "severity"].iter().find_map(|k| obj.get(*k)).and_then(jsonfile::normalize_level) {
                        rec.insert("level".into(), Value::String(l.into()));
                    }
                }
                for f in &self.cfg.promote_fields {
                    if let Some(v) = obj.get(f) {
                        let s = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        rec.insert(f.clone(), Value::String(s));
                    }
                }
            }
        }
        serde_json::to_vec(&Value::Object(rec)).ok()
    }
}

/// Moves records due for emission out of an assembler, position-free.
fn collect<P: Copy>(asm: &mut Assembler<P>, rules: &Rules, gone: bool, stopping: bool, out: &mut Vec<Record<()>>) {
    let mut recs = Vec::new();
    if gone {
        asm.flush_all(&mut recs);
    } else if stopping {
        asm.flush_groups(&mut recs);
    } else {
        asm.flush_idle(rules, &mut recs);
    }
    out.extend(recs.into_iter().map(|r| Record { stream: r.stream, time: r.time, msg: r.msg, start: (), end: () }));
}

/// The cursor to persist once the current batch is delivered.
fn commit_for(t: &Tracked) -> Option<Commit> {
    match &t.source {
        Source::File { replay: Some(_), .. } | Source::Api { replay_ns: Some(_), .. } => None,
        Source::File { read, asm, .. } => {
            let start = asm.oldest_start().unwrap_or(*read);
            let replay_until = (!asm.is_empty()).then_some(*read);
            Some(Commit::File(Saved { inode: start.inode, offset: start.offset, replay_until }))
        }
        Source::Api { last_ns, asm, .. } => {
            let since_ns = asm.oldest_start().unwrap_or(last_ns + 1);
            let replay_until_ns = (!asm.is_empty()).then_some(*last_ns);
            Some(Commit::Api(ApiSaved { since_ns, replay_until_ns }))
        }
    }
}

fn same(state: &State, id: &str, c: &Commit) -> bool {
    match c {
        Commit::File(s) => state.cursors.get(id) == Some(s),
        Commit::Api(s) => state.api.get(id) == Some(s),
    }
}

/// The collector's own container id, from /proc/self/mountinfo, so it never ships its own logs.
fn own_container_id() -> Option<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in info.lines() {
        if let Some(i) = line.find("/containers/") {
            let id: String = line[i + 12..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if id.len() == 64 {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn shipper() -> Shipper {
        let mut cfg = Config::default();
        cfg.host = "web-1".into();
        Shipper::new(&cfg, Registry::new(), Arc::new(Stats::default()))
    }

    fn fields(s: &Shipper, c: &Container) -> Map<String, Value> {
        match serde_json::from_slice(&s.encode(c, "stdout", "2026-10-10T12:00:00Z", "hi").unwrap()).unwrap() {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn compose_fields_survive_dropped_compose_labels() {
        let inspect = serde_json::from_value(serde_json::json!({
            "Id": "a".repeat(64), "Name": "/shop-web-1", "State": {"Running": true, "Pid": 0},
            "Config": {"Image": "shop/web:1", "Labels": {"com.docker.compose.project": "shop", "com.docker.compose.service": "web", "com.example.team": "a"}}
        }))
        .unwrap();
        let rules = crate::config::Containers { labels: vec![], ..Default::default() };
        let c = crate::containers::build(inspect, Path::new("/nonexistent"), Path::new("/nonexistent"), &rules);
        assert!(c.labels.is_empty());
        let f = fields(&shipper(), &c);
        assert_eq!(f["compose_project"], "shop");
        assert_eq!(f["compose_service"], "web");
        assert!(!f.contains_key("com.example.team"));
    }

    #[test]
    fn labels_never_replace_record_fields() {
        let kept: BTreeMap<String, String> = [("host", "spoofed"), ("_msg", "spoofed"), ("team", "a")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let c = Container { id: "b".repeat(64), name: "web".into(), labels: kept.clone(), all_labels: kept, ..Default::default() };
        let f = fields(&shipper(), &c);
        assert_eq!(f["host"], "web-1");
        assert_eq!(f["_msg"], "hi");
        assert_eq!(f["team"], "a");
    }
}
