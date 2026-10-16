use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimal read-only Docker Engine API client over the Unix socket.
#[derive(Clone)]
pub struct Docker {
    socket: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Summary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "State", default)]
    pub state: String,
    /// Human status, e.g. "Up 2 hours (healthy)".
    #[serde(rename = "Status", default)]
    pub status: String,
}

impl Summary {
    /// Health from `status`, in inspect's terms.
    pub fn health(&self) -> Option<&str> {
        let inner = self.status.strip_suffix(')')?.rsplit_once('(')?.1;
        match inner {
            "healthy" | "unhealthy" => Some(inner),
            "health: starting" => Some("starting"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Inspect {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Created", default)]
    pub created: String,
    #[serde(rename = "LogPath", default)]
    pub log_path: String,
    #[serde(rename = "State")]
    pub state: InspectState,
    #[serde(rename = "RestartCount", default)]
    pub restart_count: u64,
    #[serde(rename = "HostConfig", default)]
    pub host_config: HostConfig,
    #[serde(rename = "Config", default)]
    pub config: ContainerConfig,
    #[serde(rename = "GraphDriver", default)]
    pub graph_driver: GraphDriver,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphDriver {
    #[serde(rename = "Data", default)]
    pub data: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InspectState {
    #[serde(rename = "Pid", default)]
    pub pid: u32,
    #[serde(rename = "StartedAt", default)]
    pub started_at: String,
    #[serde(rename = "Status", default)]
    pub status: String,
    #[serde(rename = "ExitCode", default)]
    pub exit_code: i64,
    #[serde(rename = "OOMKilled", default)]
    pub oom_killed: bool,
    #[serde(rename = "FinishedAt", default)]
    pub finished_at: String,
    #[serde(rename = "Health", default)]
    pub health: Option<Health>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Health {
    #[serde(rename = "Status", default)]
    pub status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HostConfig {
    #[serde(rename = "NetworkMode", default)]
    pub network_mode: String,
    #[serde(rename = "LogConfig", default)]
    pub log_config: LogConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogConfig {
    #[serde(rename = "Type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Image", default)]
    pub image: String,
    #[serde(rename = "Tty", default)]
    pub tty: bool,
    #[serde(rename = "Labels", default)]
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    #[serde(rename = "Type", default)]
    pub kind: String,
    #[serde(rename = "Action", default)]
    pub action: String,
    #[serde(rename = "Actor", default)]
    pub actor: Actor,
    #[serde(rename = "timeNano", default)]
    pub time_nano: u64,
}

impl Event {
    /// Unix seconds the event happened, or `now` when Docker didn't say.
    pub fn at(&self, now: f64) -> f64 {
        if self.time_nano > 0 {
            self.time_nano as f64 / 1e9
        } else {
            now
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Actor {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "Attributes", default)]
    pub attributes: BTreeMap<String, String>,
}

pub struct Response<R> {
    pub status: u16,
    pub body: R,
}

impl Docker {
    pub fn new(socket: &Path) -> Self {
        Self { socket: socket.to_path_buf() }
    }

    fn open(&self, path: &str, timeout: Option<Duration>) -> std::io::Result<Response<Box<dyn BufRead + Send>>> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(timeout)?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        write!(s, "GET {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: collector\r\nConnection: close\r\n\r\n")?;
        let mut r = BufReader::new(s);
        let (status, headers) = read_head(&mut r)?;
        let chunked = headers.iter().any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));
        let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<u64>().ok());
        let body: Box<dyn BufRead + Send> = if chunked {
            Box::new(BufReader::new(Chunked { inner: r, left: 0, done: false }))
        } else if let Some(n) = len {
            Box::new(BufReader::new(r.take(n)))
        } else {
            Box::new(r)
        };
        Ok(Response { status, body })
    }

    /// `Ok(None)` on 404.
    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Option<T>, String> {
        let mut res = self.open(path, Some(Duration::from_secs(10))).map_err(|e| format!("docker {path}: {e}"))?;
        let mut buf = Vec::new();
        res.body.read_to_end(&mut buf).map_err(|e| format!("docker {path}: {e}"))?;
        match res.status {
            200 => serde_json::from_slice(&buf).map(Some).map_err(|e| format!("docker {path}: {e}")),
            404 => Ok(None),
            s => Err(format!("docker {path}: HTTP {s}: {}", String::from_utf8_lossy(&buf).trim())),
        }
    }

    pub fn list(&self) -> Result<Vec<Summary>, String> {
        self.get_json("/containers/json")?.ok_or_else(|| "docker /containers/json: HTTP 404".into())
    }

    /// `Ok(None)` when Docker no longer has the container.
    pub fn inspect(&self, id: &str) -> Result<Option<Inspect>, String> {
        self.get_json(&format!("/containers/{id}/json"))
    }

    /// Follows a container's log through the API, calling `f(stream, payload)` per message until the stream ends.
    pub fn follow_logs(&self, id: &str, since_ns: u64, tty: bool, stop: &std::sync::atomic::AtomicBool, mut f: impl FnMut(&'static str, &[u8]) -> bool) -> Result<(), String> {
        let since = format!("{}.{:09}", since_ns / 1_000_000_000, since_ns % 1_000_000_000);
        let path = format!("/containers/{id}/logs?follow=1&stdout=1&stderr=1&timestamps=1&since={since}");
        let mut res = self.open(&path, None).map_err(|e| format!("docker logs: {e}"))?;
        if res.status != 200 {
            return Err(format!("docker logs: HTTP {}", res.status));
        }
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            let frame = if tty { read_line_frame(&mut res.body) } else { read_mux_frame(&mut res.body) };
            match frame {
                Ok(Some((stream, payload))) => {
                    if !f(stream, &payload) {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("docker logs: {e}")),
            }
        }
    }

    /// Streams container events to `f` until the connection drops, replaying from `since_ns` when set and advancing it.
    pub fn events(&self, since_ns: &mut u64, mut f: impl FnMut(Event)) -> Result<(), String> {
        let mut path = "/events?filters=%7B%22type%22%3A%5B%22container%22%5D%7D".to_string();
        if *since_ns > 0 {
            path += &format!("&since={}.{:09}", *since_ns / 1_000_000_000, *since_ns % 1_000_000_000);
        }
        let res = self.open(&path, None).map_err(|e| format!("docker events: {e}"))?;
        if res.status != 200 {
            return Err(format!("docker events: HTTP {}", res.status));
        }
        *since_ns = (*since_ns).max(crate::util::now_secs() as u64 * 1_000_000_000);
        for line in res.body.lines() {
            let line = line.map_err(|e| format!("docker events: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                *since_ns = (*since_ns).max(ev.time_nano);
                f(ev);
            }
        }
        Err("docker events: stream ended".into())
    }
}

/// Reads one frame of Docker's multiplexed stream: 1 byte stream type, 3 zero bytes, u32 big-endian length.
pub fn read_mux_frame(r: &mut impl Read) -> std::io::Result<Option<(&'static str, Vec<u8>)>> {
    let mut head = [0u8; 8];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let stream = if head[0] == 2 { "stderr" } else { "stdout" };
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((stream, payload)))
}

/// TTY containers have no framing; every line is stdout.
fn read_line_frame(r: &mut impl BufRead) -> std::io::Result<Option<(&'static str, Vec<u8>)>> {
    let mut buf = Vec::new();
    if r.read_until(b'\n', &mut buf)? == 0 {
        return Ok(None);
    }
    Ok(Some(("stdout", buf)))
}

fn read_head(r: &mut impl BufRead) -> std::io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| bad(&format!("bad status line {line:?}")))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()));
        }
    }
    Ok((status, headers))
}

fn bad(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

/// Decodes an HTTP/1.1 chunked body.
pub struct Chunked<R> {
    pub inner: R,
    pub left: usize,
    pub done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                self.done = true;
                return Ok(0);
            }
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = usize::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let max = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        self.left -= n;
        if self.left == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

/// A canned Docker API on a Unix socket for tests.
#[cfg(test)]
pub mod fake {
    use super::Docker;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    /// Responses by path without the query; unknown paths answer 404.
    #[derive(Clone, Default)]
    pub struct Routes {
        map: Arc<Mutex<HashMap<String, (u16, String)>>>,
        /// Every request path, with its query.
        pub seen: Arc<Mutex<Vec<String>>>,
    }

    impl Routes {
        pub fn set(&self, path: &str, status: u16, body: impl Into<String>) {
            self.map.lock().unwrap().insert(path.to_string(), (status, body.into()));
        }
        pub fn hits(&self, path: &str) -> usize {
            self.seen.lock().unwrap().iter().filter(|p| p.split('?').next() == Some(path)).count()
        }
    }

    pub fn start(name: &str) -> (Docker, Routes) {
        let sock = std::env::temp_dir().join(format!("collector-fake-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock).unwrap();
        let routes = Routes::default();
        let r = routes.clone();
        std::thread::spawn(move || {
            for mut s in l.incoming().map_while(Result::ok) {
                let mut line = String::new();
                if BufReader::new(s.try_clone().unwrap()).read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                r.seen.lock().unwrap().push(path.clone());
                let key = path.split('?').next().unwrap_or("");
                let (status, body) = r.map.lock().unwrap().get(key).cloned().unwrap_or((404, "{}".into()));
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            }
        });
        (Docker::new(&sock), routes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_is_none_only_on_404() {
        let (d, r) = fake::start("inspect");
        assert!(d.inspect("gone").unwrap().is_none());
        r.set("/containers/busy/json", 500, "boom");
        assert!(d.inspect("busy").is_err());
        r.set("/containers/web/json", 200, r#"{"Id":"web","State":{}}"#);
        assert_eq!(d.inspect("web").unwrap().unwrap().id, "web");
    }

    #[test]
    fn events_resume_from_the_last_event() {
        let (d, r) = fake::start("events");
        r.set("/events", 200, "{\"Type\":\"container\",\"Action\":\"start\",\"Actor\":{\"ID\":\"a\"},\"timeNano\":4102444800000000123}\n");
        let mut since = 0;
        let mut got = Vec::new();
        assert!(d.events(&mut since, |e| got.push(e.action)).is_err());
        assert_eq!(got, ["start"]);
        assert_eq!(since, 4_102_444_800_000_000_123);
        let _ = d.events(&mut since, |_| {});
        let seen = r.seen.lock().unwrap();
        assert!(!seen[0].contains("since="));
        assert!(seen[1].ends_with("&since=4102444800.000000123"), "{}", seen[1]);
    }

    #[test]
    fn list_status_gives_health() {
        let s = |status: &str| Summary { id: String::new(), state: "running".into(), status: status.into() };
        assert_eq!(s("Up 2 hours (healthy)").health(), Some("healthy"));
        assert_eq!(s("Up 5 seconds (health: starting)").health(), Some("starting"));
        assert_eq!(s("Up 1 minute (unhealthy)").health(), Some("unhealthy"));
        assert_eq!(s("Up 3 days").health(), None);
        assert_eq!(s("Up 3 days (Paused)").health(), None);
    }

    #[test]
    fn decodes_chunked() {
        let raw = b"5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\n\r\n";
        let mut c = Chunked { inner: BufReader::new(&raw[..]), left: 0, done: false };
        let mut s = String::new();
        c.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello, world");
    }

    #[test]
    fn reads_mux_frames() {
        let mut raw = vec![1, 0, 0, 0, 0, 0, 0, 3];
        raw.extend_from_slice(b"hi\n");
        raw.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 4]);
        raw.extend_from_slice(b"oops");
        let mut r = &raw[..];
        assert_eq!(read_mux_frame(&mut r).unwrap(), Some(("stdout", b"hi\n".to_vec())));
        assert_eq!(read_mux_frame(&mut r).unwrap(), Some(("stderr", b"oops".to_vec())));
        assert_eq!(read_mux_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn parses_head() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nrest";
        let mut r = BufReader::new(&raw[..]);
        let (status, h) = read_head(&mut r).unwrap();
        assert_eq!(status, 200);
        assert!(h.contains(&("transfer-encoding".into(), "chunked".into())));
    }

    #[test]
    fn parses_inspect() {
        let j = r#"{"Id":"abc","Name":"/web","Created":"2026-01-01T00:00:00Z","LogPath":"/var/lib/docker/containers/abc/abc-json.log",
            "State":{"Pid":42,"Running":true,"StartedAt":"2026-01-01T00:00:01.5Z"},
            "HostConfig":{"NetworkMode":"bridge","LogConfig":{"Type":"json-file"}},
            "Config":{"Image":"nginx:1","Labels":{"com.docker.compose.project":"shop"}}}"#;
        let i: Inspect = serde_json::from_str(j).unwrap();
        assert_eq!(i.state.pid, 42);
        assert_eq!(i.host_config.log_config.kind, "json-file");
        assert_eq!(i.config.labels.unwrap()["com.docker.compose.project"], "shop");
        assert!(i.state.health.is_none());
        assert!(i.graph_driver.data.is_none());
        let j = r#"{"Id":"abc","State":{},"GraphDriver":{"Data":{"LowerDir":"/var/lib/docker/overlay2/x-init/diff","MergedDir":"/var/lib/docker/overlay2/x/merged","UpperDir":"/var/lib/docker/overlay2/x/diff","WorkDir":"/var/lib/docker/overlay2/x/work"},"Name":"overlay2"}}"#;
        let i: Inspect = serde_json::from_str(j).unwrap();
        assert_eq!(i.graph_driver.data.unwrap()["UpperDir"], "/var/lib/docker/overlay2/x/diff");
    }

    #[test]
    fn parses_exited_inspect_and_die_event() {
        let j = r#"{"Id":"abc","Name":"/worker","RestartCount":4,
            "State":{"Status":"exited","Running":false,"Paused":false,"Restarting":false,"OOMKilled":true,"Dead":false,"Pid":0,"ExitCode":137,
                     "StartedAt":"2026-10-10T07:02:25.289401507Z","FinishedAt":"2026-10-10T07:12:25.5Z","Health":{"Status":"unhealthy","FailingStreak":3,"Log":[]}}}"#;
        let i: Inspect = serde_json::from_str(j).unwrap();
        assert_eq!((i.state.status.as_str(), i.state.exit_code, i.state.oom_killed, i.restart_count), ("exited", 137, true, 4));
        assert_eq!(i.state.health.unwrap().status, "unhealthy");
        let e = r#"{"status":"die","id":"abc","from":"busybox","Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"exitCode":"137","image":"busybox","name":"worker"}},"scope":"local","time":1791656309,"timeNano":1791656309731000000}"#;
        let ev: Event = serde_json::from_str(e).unwrap();
        assert_eq!(ev.actor.attributes["exitCode"], "137");
        assert_eq!(ev.at(0.0), 1791656309.731);
    }
}
