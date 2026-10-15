use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub const ENV_PREFIX: &str = "COLLECTOR_";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub host: String,
    pub listen: String,
    #[serde(with = "dur")]
    pub interval: Duration,
    pub log_level: String,
    pub paths: Paths,
    pub host_metrics: HostMetrics,
    pub containers: Containers,
    pub gpu: Gpu,
    pub logs: Logs,
    pub remote_write: RemoteWrite,
    pub api: Api,
    /// How long a SIGTERM waits for in-flight log batches and queued remote-write pushes.
    #[serde(with = "dur")]
    pub shutdown_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Api {
    /// Bearer token required on /metrics and /api/v1/*; /health stays open.
    pub token: String,
    /// File holding the token, read at startup; wins over `token`.
    pub token_file: PathBuf,
    /// Threads serving requests, so one slow client can't block a scrape.
    pub workers: usize,
}

impl Default for Api {
    fn default() -> Self {
        Self { token: String::new(), token_file: PathBuf::new(), workers: 4 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub proc: PathBuf,
    pub sys: PathBuf,
    pub rootfs: PathBuf,
    pub docker_socket: PathBuf,
    pub docker_containers: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostMetrics {
    pub enabled: bool,
    pub per_cpu: bool,
    pub filesystem_exclude_types: String,
    pub filesystem_exclude_mounts: String,
    pub disk_exclude: String,
    pub network_exclude: String,
    pub hwmon: bool,
    /// Per-collection limit on statfs calls; a mount that doesn't answer is skipped until its call returns.
    #[serde(with = "dur")]
    pub statfs_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Containers {
    pub enabled: bool,
    pub network: bool,
    /// Docker labels kept on metrics, logs and the API; a trailing `.` or `*` makes it a prefix.
    pub labels: Vec<String>,
    /// Labels kept on logs and the API only, never on metrics; same matching as `labels`.
    pub api_labels: Vec<String>,
    #[serde(with = "dur")]
    pub resync: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gpu {
    pub enabled: Toggle,
    pub nvidia_smi: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSink {
    Victorialogs,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartAt {
    Beginning,
    End,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logs {
    pub enabled: bool,
    pub sink: LogSink,
    pub url: String,
    pub file: PathBuf,
    pub stream_fields: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub exclude_names: Vec<String>,
    pub exclude_label: String,
    pub first_run: StartAt,
    pub batch_bytes: usize,
    pub batch_lines: usize,
    pub read_bytes: usize,
    #[serde(with = "dur")]
    pub poll: Duration,
    #[serde(with = "dur")]
    pub max_backoff: Duration,
    #[serde(with = "dur")]
    pub timeout: Duration,
    pub parse_level: bool,
    pub promote_fields: Vec<String>,
    pub multiline: Multiline,
    /// Gzip request bodies to VictoriaLogs.
    pub compress: bool,
    /// Bound on lines queued from Docker logs API followers (local and journald drivers).
    pub api_queue_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MultilineMode {
    /// Every line is its own record.
    Off,
    /// Lines starting with a space or tab continue the previous record.
    Indent,
    /// Lines matching `pattern` continue the previous record.
    Continue,
    /// Lines matching `pattern` start a record; all others continue it.
    Start,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Multiline {
    pub mode: MultilineMode,
    pub pattern: String,
    pub max_lines: usize,
    pub max_bytes: usize,
    #[serde(with = "dur")]
    pub timeout: Duration,
}

impl Default for Multiline {
    fn default() -> Self {
        Self { mode: MultilineMode::Off, pattern: String::new(), max_lines: 500, max_bytes: 256 << 10, timeout: Duration::from_secs(2) }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteWrite {
    pub url: String,
    #[serde(with = "dur")]
    pub interval: Duration,
    #[serde(with = "dur")]
    pub timeout: Duration,
    pub labels: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    /// Pushes waiting for retry are kept up to this many compressed bytes; the oldest are dropped first.
    pub queue_bytes: usize,
    #[serde(with = "dur")]
    pub max_backoff: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            listen: "127.0.0.1:9273".into(),
            interval: Duration::from_secs(15),
            log_level: "info".into(),
            paths: Paths::default(),
            host_metrics: HostMetrics::default(),
            containers: Containers::default(),
            gpu: Gpu::default(),
            logs: Logs::default(),
            remote_write: RemoteWrite::default(),
            api: Api::default(),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            rootfs: "/".into(),
            docker_socket: "/var/run/docker.sock".into(),
            docker_containers: "/var/lib/docker/containers".into(),
            state_dir: "/var/lib/collector".into(),
        }
    }
}

impl Default for HostMetrics {
    fn default() -> Self {
        Self {
            enabled: true,
            per_cpu: true,
            filesystem_exclude_types: "^(autofs|binfmt_misc|bpf|cgroup2?|configfs|debugfs|devpts|devtmpfs|fusectl|hugetlbfs|iso9660|mqueue|nsfs|overlay|proc|procfs|pstore|rpc_pipefs|securityfs|selinuxfs|squashfs|erofs|sysfs|tracefs|tmpfs|ramfs|fuse\\..*)$".into(),
            filesystem_exclude_mounts: "^/(dev|proc|run|sys|var/lib/docker/.+|var/lib/containers/storage/.+)($|/)".into(),
            disk_exclude: "^(z?ram|loop|fd|(h|s|v|xv)d[a-z]|nvme\\d+n\\d+p)\\d+$".into(),
            network_exclude: "^(veth.*)$".into(),
            hwmon: true,
            statfs_timeout: Duration::from_secs(2),
        }
    }
}

impl Default for Containers {
    fn default() -> Self {
        Self {
            enabled: true,
            network: true,
            labels: vec![
                "com.docker.compose.project".into(),
                "com.docker.compose.service".into(),
            ],
            api_labels: Vec::new(),
            resync: Duration::from_secs(300),
        }
    }
}

impl Default for Gpu {
    fn default() -> Self {
        Self { enabled: Toggle::Auto, nvidia_smi: "nvidia-smi".into() }
    }
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            enabled: false,
            sink: LogSink::Victorialogs,
            url: "http://victorialogs:9428".into(),
            file: "/var/lib/collector/logs.jsonl".into(),
            stream_fields: vec!["host".into(), "container".into(), "compose_project".into(), "compose_service".into()],
            headers: BTreeMap::new(),
            exclude_names: vec![],
            exclude_label: "collector.logs=false".into(),
            first_run: StartAt::End,
            batch_bytes: 1 << 20,
            batch_lines: 5000,
            read_bytes: 256 << 10,
            poll: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            parse_level: true,
            promote_fields: Vec::new(),
            multiline: Multiline::default(),
            compress: true,
            api_queue_lines: 4096,
        }
    }
}

impl Default for RemoteWrite {
    fn default() -> Self {
        Self {
            url: String::new(),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            labels: BTreeMap::new(),
            headers: BTreeMap::new(),
            queue_bytes: 64 << 20,
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Config, String> {
        let mut table = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|e| format!("read {p}: {e}"))?;
                text.parse::<toml::Table>().map_err(|e| format!("parse {p}: {e}"))?
            }
            None => toml::Table::new(),
        };
        apply_env(&mut table, std::env::vars());
        let mut cfg: Config = toml::Value::Table(table).try_into().map_err(|e| format!("config: {e}"))?;
        if cfg.host.is_empty() {
            cfg.host = detect_hostname(&cfg.paths.rootfs);
        }
        if !cfg.api.token_file.as_os_str().is_empty() {
            let p = &cfg.api.token_file;
            cfg.api.token = std::fs::read_to_string(p).map_err(|e| format!("token_file {}: {e}", p.display()))?.trim().to_string();
        }
        if cfg.remote_write.labels.is_empty() {
            cfg.remote_write.labels.insert("host".into(), cfg.host.clone());
        }
        Ok(cfg)
    }
}

/// Maps COLLECTOR_SECTION__KEY=value onto [section] key = value; values parse as TOML when they can.
pub fn apply_env(table: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    for (k, v) in vars {
        let Some(rest) = k.strip_prefix(ENV_PREFIX) else { continue };
        if rest == "CONFIG" {
            continue;
        }
        let parts: Vec<String> = rest.split("__").map(|p| p.to_ascii_lowercase()).collect();
        let value = parse_env_value(&v);
        let mut cur = &mut *table;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                cur.insert(part.clone(), value.clone());
            } else {
                let entry = cur.entry(part.clone()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if !entry.is_table() {
                    *entry = toml::Value::Table(toml::Table::new());
                }
                cur = entry.as_table_mut().unwrap();
            }
        }
    }
}

fn parse_env_value(v: &str) -> toml::Value {
    let wrapped = format!("v = {v}");
    match wrapped.parse::<toml::Table>() {
        Ok(mut t) => t.remove("v").unwrap_or_else(|| toml::Value::String(v.into())),
        Err(_) => toml::Value::String(v.into()),
    }
}

fn detect_hostname(rootfs: &std::path::Path) -> String {
    for p in [rootfs.join("etc/hostname"), PathBuf::from("/etc/hostname")] {
        if let Ok(s) = std::fs::read_to_string(&p) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        return String::from_utf8_lossy(&buf[..end]).into_owned();
    }
    "unknown".into()
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" | "" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(format!("bad duration unit in {s:?}")),
    };
    Ok(Duration::from_secs_f64(secs))
}

mod dur {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            N(u64),
        }
        match Raw::deserialize(d)? {
            Raw::S(s) => super::parse_duration(&s).map_err(serde::de::Error::custom),
            Raw::N(n) => Ok(Duration::from_secs(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("15s").unwrap(), Duration::from_secs(15));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("3x").is_err());
    }

    #[test]
    fn env_overrides_nested_keys() {
        let mut t: toml::Table = "listen = \"127.0.0.1:1\"\n[logs]\nenabled = false\n".parse().unwrap();
        let vars = vec![
            ("COLLECTOR_LISTEN".to_string(), "0.0.0.0:2".to_string()),
            ("COLLECTOR_LOGS__ENABLED".to_string(), "true".to_string()),
            ("COLLECTOR_LOGS__URL".to_string(), "http://192.0.2.10:9428".to_string()),
            ("COLLECTOR_REMOTE_WRITE__URL".to_string(), "http://192.0.2.11/api/v1/write".to_string()),
            ("OTHER".to_string(), "x".to_string()),
        ];
        apply_env(&mut t, vars.into_iter());
        let cfg: Config = toml::Value::Table(t).try_into().unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:2");
        assert!(cfg.logs.enabled);
        assert_eq!(cfg.logs.url, "http://192.0.2.10:9428");
        assert_eq!(cfg.remote_write.url, "http://192.0.2.11/api/v1/write");
    }

    #[test]
    fn rejects_unknown_keys() {
        let t: toml::Table = "nope = 1".parse().unwrap();
        assert!(toml::Value::Table(t).try_into::<Config>().is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../collector.example.toml");
        let t: toml::Table = text.parse().unwrap();
        toml::Value::Table(t).try_into::<Config>().unwrap();
    }
}
