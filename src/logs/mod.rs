pub mod assemble;
pub mod cursor;
pub mod jsonfile;
pub mod sink;

use crate::config::{Config, StartAt};
use crate::containers::{Container, Registry};
use crate::metrics::MetricSet;
use crate::util::{self, log, shutdown};
use assemble::{Assembler, Piece};
use cursor::{Cursor, Start, State, Tail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    pub partials_split: AtomicU64,
    pub followed: AtomicU64,
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
        m.counter("collector_log_rotations_missed_total", "Log files that rotated away before they were read.", &[], g(&self.lost_rotations));
        m.counter("collector_log_partials_split_total", "Unfinished long lines sent in two or more records after logs.partial_timeout.", &[], g(&self.partials_split));
        m.gauge("collector_log_containers_followed", "Containers whose log files are followed.", &[], g(&self.followed));
        m.gauge("collector_log_containers_unsupported", "Running containers whose log driver isn't json-file.", &[], g(&self.unsupported));
        m.gauge("collector_log_last_success_timestamp_seconds", "Time of the last delivered batch.", &[], g(&self.last_success));
    }
}

struct Tracked {
    meta: Arc<Container>,
    tenant: u32,
    tail: Tail,
    asm: Assembler<Cursor>,
    gone: bool,
    /// Gone, and everything it wrote was delivered.
    done: bool,
}

/// One tenant's next request. Once a send fails it's retried as is, and that tenant's containers aren't read meanwhile.
struct Batch {
    body: Vec<u8>,
    lines: usize,
    /// Cursors saved once the sink accepts `body`.
    commits: Vec<(String, Cursor)>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl Batch {
    fn new() -> Self {
        Self { body: Vec::new(), lines: 0, commits: Vec::new(), retry_at: None, backoff: Duration::from_millis(500) }
    }

    fn push(&mut self, rec: &[u8]) {
        self.body.extend_from_slice(rec);
        self.body.push(b'\n');
        self.lines += 1;
    }

    fn commit(&mut self, id: &str, at: Cursor) {
        match self.commits.iter_mut().find(|(i, _)| i == id) {
            Some(c) => c.1 = at,
            None => self.commits.push((id.to_string(), at)),
        }
    }

    fn is_empty(&self) -> bool {
        self.lines == 0 && self.commits.is_empty()
    }
}

pub struct Shipper {
    cfg: crate::config::Logs,
    host: String,
    containers_dir: PathBuf,
    state_path: PathBuf,
    registry: Registry,
    stats: Arc<Stats>,
    exclude: Vec<regex_lite::Regex>,
    exclude_label: Option<(String, String)>,
    self_id: Option<String>,
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
            stats,
            exclude,
            exclude_label,
            self_id: own_container_id(),
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
        let (mut state, first_run) = load_state(&self.state_path);
        state.version = 2;
        while !self.registry.synced() {
            if shutdown::sleep(Duration::from_millis(200)) {
                self.stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
        }
        let mut tracked: HashMap<String, Tracked> = HashMap::new();
        let mut batches: BTreeMap<u32, Batch> = BTreeMap::new();
        let mut warned: HashSet<String> = HashSet::new();
        let mut first = true;
        let mut rr = 0usize;
        let mut last_prune = Instant::now();
        loop {
            if shutdown::requested() {
                if !self.finish(&mut *sink, &mut batches, &mut state) {
                    log::warn!("logs: undelivered lines will be re-read on restart");
                }
                if let Err(e) = state.save(&self.state_path) {
                    log::warn!("logs: saving cursors: {e}");
                }
                log::info!("logs: stopped, cursors saved");
                self.stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
            self.refresh(&mut tracked, &mut state, &mut warned, first && first_run);
            if first {
                let _ = state.save(&self.state_path);
                first = false;
            }
            self.drain(&mut *sink, &mut tracked, &mut batches, &mut state, &mut rr);
            tracked.retain(|_, t| !t.done);
            if last_prune.elapsed() > Duration::from_secs(600) {
                prune(&mut state, &tracked, &self.containers_dir);
                let _ = state.save(&self.state_path);
                last_prune = Instant::now();
            }
            shutdown::sleep(self.cfg.poll);
        }
    }

    /// Updates the follow set from the registry; containers that left are read to the end, then dropped.
    fn refresh(&self, tracked: &mut HashMap<String, Tracked>, state: &mut State, warned: &mut HashSet<String>, initial: bool) {
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
            if !readable(&c) {
                unsupported += 1;
                if warned.insert(c.id.clone()) {
                    log::warn!("logs: {} uses the {} log driver; only json-file is read, so its logs are not shipped", c.name, c.log_driver);
                }
                continue;
            }
            let start = if initial && self.cfg.first_run == StartAt::End { Start::End } else { Start::Beginning };
            let Some((tail, lost)) = Tail::open(&self.log_file(&c), state.cursors.get(&c.id).copied(), start) else { continue };
            if lost {
                self.stats.lost_rotations.fetch_add(1, Ordering::Relaxed);
                log::warn!("logs: {} rotated past its saved cursor; some lines were not shipped", c.name);
            }
            state.cursors.entry(c.id.clone()).or_insert(tail.cursor);
            let tenant = self.tenant(&c, warned);
            tracked.insert(c.id.clone(), Tracked { meta: c, tenant, tail, asm: Assembler::default(), gone: false, done: false });
        }
        self.stats.followed.store(tracked.values().filter(|t| !t.gone).count() as u64, Ordering::Relaxed);
        self.stats.unsupported.store(unsupported, Ordering::Relaxed);
    }

    /// The container's tenant from `logs.tenant_label`; 0 when unset, unlabeled or not a number.
    fn tenant(&self, c: &Container, warned: &mut HashSet<String>) -> u32 {
        if self.cfg.tenant_label.is_empty() {
            return 0;
        }
        let Some(v) = c.all_labels.get(&self.cfg.tenant_label) else { return 0 };
        v.trim().parse().unwrap_or_else(|_| {
            if warned.insert(format!("tenant:{}", c.id)) {
                log::warn!("logs: {} has {}={v:?}, which isn't a numeric AccountID; using tenant 0", c.name, self.cfg.tenant_label);
            }
            0
        })
    }

    /// Reads and sends, one request per tenant per round, until caught up or every tenant with data is waiting to retry.
    fn drain(&self, sink: &mut dyn sink::Sink, tracked: &mut HashMap<String, Tracked>, batches: &mut BTreeMap<u32, Batch>, state: &mut State, rr: &mut usize) {
        let mut ids: Vec<String> = tracked.keys().cloned().collect();
        ids.sort();
        let n = ids.len();
        loop {
            let mut more = false;
            let mut resume = None;
            for k in 0..n {
                let i = (*rr + k) % n;
                let t = tracked.get_mut(&ids[i]).unwrap();
                let b = batches.entry(t.tenant).or_insert_with(Batch::new);
                if b.retry_at.is_some() {
                    continue;
                }
                if b.body.len() >= self.cfg.batch_bytes || b.lines >= self.cfg.batch_lines {
                    resume.get_or_insert(i);
                    more = true;
                    continue;
                }
                more |= self.read(&ids[i], t, b);
            }
            *rr = resume.unwrap_or(*rr);
            let retried = self.send_due(sink, batches, state);
            if !(more || retried) || shutdown::requested() {
                return;
            }
        }
    }

    /// Reads one chunk of a container into its tenant's batch; true when more is waiting.
    fn read(&self, id: &str, t: &mut Tracked, b: &mut Batch) -> bool {
        let chunk = t.tail.read(self.cfg.read_bytes);
        if chunk.lost > 0 {
            self.stats.lost_rotations.fetch_add(chunk.lost, Ordering::Relaxed);
            log::warn!("logs: {} rotated {} file(s) away before they were read; their lines were not shipped", t.meta.name, chunk.lost);
        }
        let mut recs = Vec::new();
        for l in &chunk.lines {
            let Some(parsed) = jsonfile::parse_line(chunk.line(l)) else {
                self.stats.unparsed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            t.asm.push(Piece { stream: &parsed.stream, time: &parsed.time, text: &parsed.log, start: l.start, end: l.end }, &mut recs);
        }
        if t.asm.release_stale(self.cfg.partial_timeout, &mut recs) {
            self.stats.partials_split.fetch_add(1, Ordering::Relaxed);
        }
        if t.gone && chunk.eof {
            if chunk.lines.is_empty() && t.asm.oldest_start().is_none() {
                t.done = true;
            }
            t.asm.flush(&mut recs);
        }
        for r in &recs {
            if let Some(rec) = self.encode(&t.meta, &r.stream, &r.time, &r.msg) {
                b.push(&rec);
            }
        }
        if !chunk.lines.is_empty() || !recs.is_empty() {
            b.commit(id, t.asm.oldest_start().unwrap_or(t.tail.cursor));
        }
        !chunk.eof
    }

    /// Sends each tenant's batch that's due once; delivered batches commit their cursors. True when a retried batch got through.
    fn send_due(&self, sink: &mut dyn sink::Sink, batches: &mut BTreeMap<u32, Batch>, state: &mut State) -> bool {
        let now = Instant::now();
        let mut changed = false;
        let mut retried = false;
        for (tenant, b) in batches.iter_mut() {
            if b.is_empty() || b.retry_at.is_some_and(|r| r > now) {
                continue;
            }
            if b.lines > 0 {
                let account = (!self.cfg.tenant_label.is_empty()).then_some(*tenant);
                if let Err(e) = sink.send(&b.body, account) {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    let who = account.map(|t| format!("tenant {t}: ")).unwrap_or_default();
                    log::warn!("logs: {who}{e}; retrying in {:.1}s", b.backoff.as_secs_f64());
                    b.retry_at = Some(now + b.backoff);
                    b.backoff = (b.backoff * 2).min(self.cfg.max_backoff);
                    continue;
                }
                self.stats.lines.fetch_add(b.lines as u64, Ordering::Relaxed);
                self.stats.bytes.fetch_add(b.body.len() as u64, Ordering::Relaxed);
                self.stats.batches.fetch_add(1, Ordering::Relaxed);
                self.stats.last_success.store(util::now_secs() as u64, Ordering::Relaxed);
            }
            for (id, c) in b.commits.drain(..) {
                changed |= state.cursors.insert(id, c) != Some(c);
            }
            retried |= b.retry_at.is_some();
            *b = Batch::new();
        }
        batches.retain(|_, b| !b.is_empty());
        if changed {
            if let Err(e) = state.save(&self.state_path) {
                log::warn!("logs: saving cursors: {e}");
            }
        }
        retried
    }

    /// Retries undelivered batches until they're delivered or `shutdown_timeout` passes.
    fn finish(&self, sink: &mut dyn sink::Sink, batches: &mut BTreeMap<u32, Batch>, state: &mut State) -> bool {
        let deadline = Instant::now() + self.shutdown_timeout;
        loop {
            self.send_due(sink, batches, state);
            if batches.is_empty() {
                return true;
            }
            let next = batches.values().filter_map(|b| b.retry_at).min().unwrap_or(deadline);
            if next >= deadline {
                return false;
            }
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
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

fn readable(c: &Container) -> bool {
    matches!(c.log_driver.as_str(), "json-file" | "")
}

/// The saved cursors and whether this is a first run. An unreadable file is kept aside and every container reads from its oldest file.
fn load_state(path: &Path) -> (State, bool) {
    match State::load(path) {
        Ok(Some(s)) => (s, false),
        Ok(None) => (State::default(), true),
        Err(e) => {
            let bad = path.with_extension(format!("json.bad-{}", util::now_secs() as u64));
            let kept = match std::fs::rename(path, &bad) {
                Ok(()) => format!("kept as {}", bad.display()),
                Err(re) => format!("could not keep it: {re}"),
            };
            log::error!("logs: {} is unreadable ({e}); {kept}; re-reading every container's logs from its oldest file", path.display());
            (State::default(), false)
        }
    }
}

/// Drops cursors of containers that are neither followed nor still have a log dir.
fn prune(state: &mut State, tracked: &HashMap<String, Tracked>, containers_dir: &Path) {
    state.cursors.retain(|id, _| tracked.contains_key(id) || containers_dir.join(id).exists());
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
    use std::sync::{Mutex, PoisonError};

    fn shipper() -> Shipper {
        let cfg = Config { host: "web-1".into(), ..Default::default() };
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

    #[test]
    fn only_json_file_is_readable() {
        for (driver, ok) in [("json-file", true), ("", true), ("local", false), ("journald", false), ("syslog", false)] {
            assert_eq!(readable(&Container { log_driver: driver.into(), ..Default::default() }), ok, "{driver}");
        }
    }

    type Sent = Vec<(Option<u32>, Vec<String>)>;

    /// Records each request's tenant and messages; fails every request for `fail`.
    struct Recorder {
        sent: Arc<Mutex<Sent>>,
        fail: Option<u32>,
    }

    impl sink::Sink for Recorder {
        fn send(&mut self, body: &[u8], tenant: Option<u32>) -> Result<(), String> {
            if Some(tenant.unwrap_or(0)) == self.fail {
                return Err("HTTP 400".into());
            }
            let msgs = String::from_utf8_lossy(body).lines().map(|l| serde_json::from_str::<Value>(l).unwrap()["_msg"].as_str().unwrap().to_string()).collect();
            self.sent.lock().unwrap_or_else(PoisonError::into_inner).push((tenant, msgs));
            Ok(())
        }
    }

    struct Rig {
        root: PathBuf,
        shipper: Shipper,
        tracked: HashMap<String, Tracked>,
        batches: BTreeMap<u32, Batch>,
        state: State,
        sent: Arc<Mutex<Sent>>,
    }

    fn line(stream: &str, text: &str) -> String {
        format!("{}\n", serde_json::json!({"log": text, "stream": stream, "time": "2026-10-10T12:00:00Z"}))
    }

    impl Rig {
        fn new(name: &str, tenant_label: &str) -> Rig {
            let root = std::env::temp_dir().join(format!("collector-logs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let mut cfg = Config { host: "web-1".into(), ..Default::default() };
            cfg.paths.state_dir = root.join("state");
            cfg.paths.docker_containers = root.clone();
            cfg.logs.tenant_label = tenant_label.into();
            std::fs::create_dir_all(&cfg.paths.state_dir).unwrap();
            let shipper = Shipper::new(&cfg, Registry::new(), Arc::new(Stats::default()));
            Rig { root, shipper, tracked: HashMap::new(), batches: BTreeMap::new(), state: State::default(), sent: Arc::default() }
        }

        fn path(&self, id: &str) -> PathBuf {
            self.root.join(format!("{id}-json.log"))
        }

        fn write(&self, id: &str, lines: &[(&str, &str)]) {
            let text: String = lines.iter().map(|(s, t)| line(s, t)).collect();
            std::fs::OpenOptions::new().create(true).append(true).open(self.path(id)).unwrap().write_all(text.as_bytes()).unwrap();
        }

        /// Follows `id` from its saved cursor, as after a restart.
        fn follow(&mut self, id: &str, name: &str, tenant: Option<&str>) {
            let all_labels = tenant.iter().map(|v| ("com.example.tenant".to_string(), v.to_string())).collect();
            let c = Container { id: id.into(), name: name.into(), all_labels, ..Default::default() };
            let (tail, _) = Tail::open(&self.path(id), self.state.cursors.get(id).copied(), Start::Beginning).unwrap();
            let tenant = self.shipper.tenant(&c, &mut HashSet::new());
            self.tracked.insert(id.into(), Tracked { meta: Arc::new(c), tenant, tail, asm: Assembler::default(), gone: false, done: false });
        }

        fn drain(&mut self, fail: Option<u32>) -> Sent {
            let mut sink = Recorder { sent: self.sent.clone(), fail };
            self.shipper.drain(&mut sink, &mut self.tracked, &mut self.batches, &mut self.state, &mut 0);
            std::mem::take(&mut *self.sent.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    use std::io::Write;

    fn tenants_rig(name: &str, tenant_label: &str, tenants: &[(&str, Option<&str>)]) -> Rig {
        let mut rig = Rig::new(name, tenant_label);
        for (i, (name, label)) in tenants.iter().enumerate() {
            let id = format!("{i:x}").repeat(64);
            rig.write(&id, &[("stdout", &format!("hello from {name}\n"))]);
            rig.follow(&id, name, *label);
        }
        rig
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_request_per_tenant_per_flush() {
        let mut rig = tenants_rig("tenants", "com.example.tenant", &[("a", Some("7")), ("b", Some("3")), ("c", None), ("d", Some("7")), ("e", Some("x"))]);
        let want = vec![
            (Some(0), strs(&["hello from c", "hello from e"])),
            (Some(3), strs(&["hello from b"])),
            (Some(7), strs(&["hello from a", "hello from d"])),
        ];
        assert_eq!(rig.drain(None), want);
    }

    #[test]
    fn without_tenant_label_one_request_without_account_id() {
        let mut rig = tenants_rig("no-tenant", "", &[("a", Some("7")), ("b", None)]);
        assert_eq!(rig.drain(None), vec![(None, strs(&["hello from a", "hello from b"]))]);
    }

    #[test]
    fn a_failing_tenant_holds_back_only_its_own_containers() {
        let mut rig = tenants_rig("failing", "com.example.tenant", &[("a", Some("7")), ("b", Some("3")), ("c", None)]);
        let ids: Vec<String> = (0..3).map(|i| format!("{i:x}").repeat(64)).collect();
        assert_eq!(rig.drain(Some(3)), vec![(Some(0), strs(&["hello from c"])), (Some(7), strs(&["hello from a"]))]);
        assert_eq!(rig.state.cursors[&ids[0]].offset, std::fs::metadata(rig.path(&ids[0])).unwrap().len());
        assert!(!rig.state.cursors.contains_key(&ids[1]));
        rig.write(&ids[0], &[("stdout", "more from a\n")]);
        rig.write(&ids[1], &[("stdout", "more from b\n")]);
        assert_eq!(rig.drain(Some(3)), vec![(Some(7), strs(&["more from a"]))]);
        rig.batches.get_mut(&3).unwrap().retry_at = Some(Instant::now());
        assert_eq!(rig.drain(None), vec![(Some(3), strs(&["hello from b"])), (Some(3), strs(&["more from b"]))]);
        assert_eq!(rig.state.cursors[&ids[1]].offset, std::fs::metadata(rig.path(&ids[1])).unwrap().len());
    }

    #[test]
    fn partial_lines_are_delivered_once_across_a_restart() {
        let mut rig = Rig::new("partial", "");
        let id = "c".repeat(64);
        rig.write(&id, &[("stdout", "first\n"), ("stdout", "big-1 "), ("stderr", "err-1\n"), ("stdout", "big-2 ")]);
        rig.follow(&id, "app", None);
        assert_eq!(rig.drain(None), vec![(None, strs(&["first"]))]);
        rig.write(&id, &[("stdout", "big-3\n"), ("stderr", "err-2\n")]);
        rig.tracked.clear();
        rig.follow(&id, "app", None);
        assert_eq!(rig.drain(None), vec![(None, strs(&["big-1 big-2 big-3", "err-1", "err-2"]))]);
        rig.tracked.clear();
        rig.follow(&id, "app", None);
        assert_eq!(rig.drain(None), vec![]);
    }

    #[test]
    fn gone_containers_flush_held_pieces_and_are_dropped_once_delivered() {
        let mut rig = Rig::new("gone", "");
        let id = "d".repeat(64);
        rig.write(&id, &[("stdout", "unfinished")]);
        rig.follow(&id, "app", None);
        assert_eq!(rig.drain(None), vec![]);
        rig.tracked.get_mut(&id).unwrap().gone = true;
        assert_eq!(rig.drain(Some(0)), vec![]);
        assert!(!rig.tracked[&id].done);
        rig.batches.get_mut(&0).unwrap().retry_at = Some(Instant::now());
        assert_eq!(rig.drain(None), vec![(None, strs(&["unfinished"]))]);
        assert!(rig.tracked[&id].done);
    }

    #[test]
    fn unreadable_state_is_kept_aside_and_not_a_first_run() {
        let rig = Rig::new("state", "");
        let p = rig.root.join("log-cursors.json");
        assert!(load_state(&p).1);
        std::fs::write(&p, "{\"version\":2,\"cur").unwrap();
        let (state, first_run) = load_state(&p);
        assert!(!first_run);
        assert!(state.cursors.is_empty());
        let kept: Vec<_> = std::fs::read_dir(&rig.root).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("log-cursors.json.bad-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(kept[0].path()).unwrap(), "{\"version\":2,\"cur");
    }

    #[test]
    fn prune_keeps_cursors_while_the_log_dir_exists() {
        let rig = Rig::new("prune", "");
        let (stopped, removed) = ("e".repeat(64), "f".repeat(64));
        std::fs::create_dir_all(rig.root.join(&stopped)).unwrap();
        let mut state = State::default();
        for id in [&stopped, &removed] {
            state.cursors.insert(id.clone(), Cursor { inode: 1, offset: 1 });
        }
        prune(&mut state, &HashMap::new(), &rig.root);
        assert_eq!(state.cursors.keys().collect::<Vec<_>>(), [&stopped]);
    }
}
