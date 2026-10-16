use crate::config::{Compat, RemoteWrite};
use crate::metrics::{family_name, MetricSet};
use crate::state::{RemoteStats, Shared};
use crate::util::{self, log, shutdown};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Encodes a Prometheus remote-write WriteRequest (protobuf), one sample per series.
pub fn encode(m: &MetricSet, extra: &[(String, String)], ts_ms: i64, mode: Compat) -> Vec<u8> {
    let mut out = Vec::with_capacity(m.len() * 128);
    let mut series = Vec::with_capacity(256);
    let mut labels: Vec<(&str, &str)> = Vec::with_capacity(16);
    for f in &m.families {
        for s in &f.samples {
            labels.clear();
            labels.push(("__name__", family_name(&f.name, mode)));
            for (k, v) in extra.iter().chain(s.all_labels(mode)) {
                if !labels.iter().any(|(lk, _)| *lk == k) {
                    labels.push((k, v));
                }
            }
            labels.sort_by(|a, b| a.0.cmp(b.0));
            series.clear();
            for (k, v) in &labels {
                let mut lb = Vec::with_capacity(k.len() + v.len() + 4);
                bytes_field(&mut lb, 1, k.as_bytes());
                bytes_field(&mut lb, 2, v.as_bytes());
                bytes_field(&mut series, 1, &lb);
            }
            let mut sample = Vec::with_capacity(20);
            sample.push(0x09);
            sample.extend_from_slice(&s.value.to_le_bytes());
            sample.push(0x10);
            varint(&mut sample, ts_ms as u64);
            bytes_field(&mut series, 2, &sample);
            bytes_field(&mut out, 1, &series);
        }
    }
    out
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn bytes_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    varint(out, ((field << 3) | 2) as u64);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
    labels: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    mode: Compat,
}

impl Client {
    pub fn new(cfg: &RemoteWrite, mode: Compat) -> Self {
        Self {
            mode,
            agent: ureq::AgentBuilder::new().timeout(cfg.timeout).build(),
            url: cfg.url.clone(),
            labels: cfg.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }

    /// Encodes one WriteRequest, snappy-compressed as the protocol requires.
    pub fn encode(&self, m: &MetricSet, ts_ms: i64) -> Result<Vec<u8>, String> {
        snap::raw::Encoder::new().compress_vec(&encode(m, &self.labels, ts_ms, self.mode)).map_err(|e| e.to_string())
    }

    pub fn send(&self, body: &[u8]) -> Result<(), SendError> {
        let mut req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/x-protobuf")
            .set("Content-Encoding", "snappy")
            .set("X-Prometheus-Remote-Write-Version", "0.1.0");
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        match req.send_bytes(body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, r)) => {
                let msg = format!("remote write: HTTP {code}: {}", r.into_string().unwrap_or_default().trim());
                Err(if (400..500).contains(&code) && code != 429 { SendError::Rejected(msg) } else { SendError::Retry(msg) })
            }
            Err(e) => Err(SendError::Retry(format!("remote write: {e}"))),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum SendError {
    /// 5xx, 429 or transport errors.
    Retry(String),
    /// Other 4xx; resending won't help.
    Rejected(String),
}

/// Sends from the queue head until it's empty or a push should be retried; returns that push's error.
fn drain(client: &Client, queue: &mut Queue, stats: &RemoteStats) -> Option<String> {
    while let Some(p) = queue.front() {
        match client.send(&p.body) {
            Ok(()) => {
                stats.pushes.fetch_add(1, Ordering::Relaxed);
                stats.last_success.store(util::now_secs() as u64, Ordering::Relaxed);
            }
            Err(SendError::Rejected(e)) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                stats.dropped_samples.fetch_add(p.samples as u64, Ordering::Relaxed);
                log::warn!("{e}; dropped {} samples", p.samples);
            }
            Err(SendError::Retry(e)) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                return Some(e);
            }
        }
        queue.pop();
    }
    None
}

struct Pending {
    body: Vec<u8>,
    samples: usize,
}

/// Bounded FIFO of encoded pushes; the oldest go first when it's full.
pub struct Queue {
    items: VecDeque<Pending>,
    bytes: usize,
    max_bytes: usize,
}

impl Queue {
    pub fn new(max_bytes: usize) -> Self {
        Self { items: VecDeque::new(), bytes: 0, max_bytes }
    }

    /// Adds a push and returns how many samples were dropped to make room.
    pub fn push(&mut self, body: Vec<u8>, samples: usize) -> usize {
        self.bytes += body.len();
        self.items.push_back(Pending { body, samples });
        let mut dropped = 0;
        while self.bytes > self.max_bytes && self.items.len() > 1 {
            let old = self.items.pop_front().unwrap();
            self.bytes -= old.body.len();
            dropped += old.samples;
        }
        dropped
    }

    fn front(&self) -> Option<&Pending> {
        self.items.front()
    }

    fn pop(&mut self) {
        if let Some(p) = self.items.pop_front() {
            self.bytes -= p.body.len();
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn samples(&self) -> usize {
        self.items.iter().map(|p| p.samples).sum()
    }
}

/// Encodes each new collection into the queue and sends from its head with backoff; on shutdown, drains until the deadline.
pub fn run(cfg: RemoteWrite, mode: Compat, shared: Shared, shutdown_timeout: Duration) {
    let client = Client::new(&cfg, mode);
    let mut queue = Queue::new(cfg.queue_bytes);
    let stats = shared.remote.clone();
    let mut last = 0.0;
    let mut next_enqueue = Instant::now() + cfg.interval;
    let mut next_send = Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut deadline: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if deadline.is_none() && shutdown::requested() {
            deadline = Some(now + shutdown_timeout);
            next_send = now;
        }
        if deadline.is_none() && now >= next_enqueue {
            next_enqueue = now + cfg.interval;
            let (m, at) = {
                let s = shared.read();
                (s.metrics.clone(), s.collected_at)
            };
            if at > last {
                last = at;
                match client.encode(&m, (at * 1000.0) as i64) {
                    Ok(body) => {
                        let dropped = queue.push(body, m.len());
                        if dropped > 0 {
                            stats.dropped_samples.fetch_add(dropped as u64, Ordering::Relaxed);
                            log::warn!("remote write: queue full, dropped {dropped} samples");
                        }
                    }
                    Err(e) => log::warn!("remote write: encode: {e}"),
                }
            }
        }
        if Instant::now() >= next_send {
            match drain(&client, &mut queue, &stats) {
                None => backoff = Duration::from_secs(1),
                Some(e) => {
                    log::warn!("{e}; {} pushes queued, retrying in {}s", queue.len(), backoff.as_secs());
                    next_send = Instant::now() + backoff;
                    backoff = (backoff * 2).min(cfg.max_backoff);
                }
            }
        }
        stats.queue_len.store(queue.len() as u64, Ordering::Relaxed);
        stats.queue_bytes.store(queue.bytes as u64, Ordering::Relaxed);
        if let Some(d) = deadline {
            if queue.len() == 0 || Instant::now() >= d {
                let left = queue.samples();
                if left > 0 {
                    stats.dropped_samples.fetch_add(left as u64, Ordering::Relaxed);
                    log::warn!("remote write: shutting down with {left} samples unsent");
                }
                stats.stopped.store(true, Ordering::SeqCst);
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_drops_oldest_when_full() {
        let mut q = Queue::new(25);
        assert_eq!(q.push(vec![0; 10], 1), 0);
        assert_eq!(q.push(vec![0; 10], 2), 0);
        assert_eq!(q.push(vec![0; 10], 3), 1);
        assert_eq!(q.len(), 2);
        assert_eq!(q.samples(), 5);
        q.pop();
        assert_eq!(q.samples(), 3);
        // A single oversized push is kept rather than dropped outright.
        let mut q = Queue::new(5);
        assert_eq!(q.push(vec![0; 10], 7), 0);
        assert_eq!(q.len(), 1);
    }

    /// Answers each request with the next status in `codes`.
    fn receiver(codes: &'static [u16]) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1/write", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for &code in codes {
                let (s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                r.read_exact(&mut vec![0; len]).unwrap();
                write!(&s, "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        url
    }

    #[test]
    fn drops_rejected_pushes_and_keeps_retryable_ones() {
        let cfg = RemoteWrite { url: receiver(&[400, 204, 429, 503]), ..Default::default() };
        let client = Client::new(&cfg, Compat::None);
        let stats = RemoteStats::default();
        let mut q = Queue::new(1 << 20);
        q.push(vec![1], 10);
        q.push(vec![2], 20);
        q.push(vec![3], 30);
        // 400 drops the first push, 204 sends the second, 429 keeps the third.
        assert!(drain(&client, &mut q, &stats).unwrap().contains("HTTP 429"));
        assert_eq!((q.len(), q.samples()), (1, 30));
        assert_eq!(stats.dropped_samples.load(Ordering::Relaxed), 10);
        assert_eq!(stats.pushes.load(Ordering::Relaxed), 1);
        assert_eq!(stats.errors.load(Ordering::Relaxed), 2);
        assert!(drain(&client, &mut q, &stats).unwrap().contains("HTTP 503"));
        assert_eq!(q.len(), 1);
        assert_eq!(stats.dropped_samples.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn transport_errors_are_retried() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = RemoteWrite { url: format!("http://{}/", l.local_addr().unwrap()), ..Default::default() };
        drop(l);
        assert!(matches!(Client::new(&cfg, Compat::None).send(b"x"), Err(SendError::Retry(_))));
    }

    #[test]
    fn varints() {
        let mut v = Vec::new();
        varint(&mut v, 300);
        assert_eq!(v, [0xac, 0x02]);
    }

    #[test]
    fn encodes_one_series() {
        let mut m = MetricSet::new();
        m.gauge("up", "Up.", &[("job", "x")], 1.0);
        let b = encode(&m, &[("host".into(), "h".into())], 1000, Compat::None);
        // WriteRequest.timeseries
        assert_eq!(b[0], 0x0a);
        let body = &b[2..];
        assert_eq!(b[1] as usize, body.len());
        // First label is __name__="up", labels sorted after it: host, job.
        let expected_label = [0x0a, 0x0e, 0x0a, 0x08, b'_', b'_', b'n', b'a', b'm', b'e', b'_', b'_', 0x12, 0x02, b'u', b'p'];
        assert_eq!(&body[..expected_label.len()], &expected_label);
        let host_at = body.windows(4).position(|w| w == b"host").unwrap();
        let job_at = body.windows(3).position(|w| w == b"job").unwrap();
        assert!(host_at < job_at);
        // Sample: value 1.0 as fixed64, timestamp 1000 as varint.
        let mut sample = vec![0x12, 0x0c, 0x09];
        sample.extend_from_slice(&1.0f64.to_le_bytes());
        sample.extend_from_slice(&[0x10, 0xe8, 0x07]);
        assert!(body.ends_with(&sample));
    }

    #[test]
    fn cadvisor_mode_renames_pushed_series() {
        use crate::metrics::{Base, Kind};
        let mut m = MetricSet::new();
        let base = std::sync::Arc::new(Base { clean: vec![("container".into(), "web".into())], cadvisor: vec![("name".into(), "web".into())] });
        m.push_with("container_memory_swap_bytes", "S.", Kind::Gauge, &base, &[], 1.0);
        let has = |b: &[u8], s: &str| b.windows(s.len()).any(|w| w == s.as_bytes());
        // Label names as encoded: length byte, name, then the value's tag.
        let (container, name) = ("\x09container\x12", "\x04name\x12");
        let clean = encode(&m, &[], 0, Compat::None);
        assert!(has(&clean, "container_memory_swap_bytes") && has(&clean, container) && !has(&clean, name));
        let old = encode(&m, &[], 0, Compat::Cadvisor);
        assert!(has(&old, "container_memory_swap") && !has(&old, "swap_bytes") && has(&old, name) && !has(&old, container));
    }

    #[test]
    fn extra_labels_do_not_duplicate() {
        let mut m = MetricSet::new();
        m.gauge("x", "X.", &[("host", "own")], 1.0);
        let b = encode(&m, &[("host".into(), "cfg".into())], 0, Compat::None);
        assert_eq!(b.windows(4).filter(|w| *w == b"host").count(), 1);
    }
}
