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
    /// Collection interval while a client follows `/api/v1/stream`.
    #[serde(with = "dur")]
    pub live_interval: Duration,
    pub log_level: String,
    pub paths: Paths,
    pub host_metrics: HostMetrics,
    pub containers: Containers,
    pub gpu: Gpu,
    pub logs: Logs,
    pub remote_write: RemoteWrite,
    pub api: Api,
    pub metrics: Metrics,
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
    /// `auto` uses the token when one is set, `token` requires one, `off` serves openly without the warning.
    pub auth: Auth,
}

impl Default for Api {
    fn default() -> Self {
        Self { token: String::new(), token_file: PathBuf::new(), workers: 4, auth: Auth::Auto }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    Auto,
    Token,
    Off,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    /// Names and labels for remote write; scrapes choose with `?compat=`.
    pub compat: Compat,
}

/// Metric naming: clean names, or cAdvisor's names and labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compat {
    #[default]
    None,
    Cadvisor,
}

impl Compat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "none" => Some(Compat::None),
            "cadvisor" => Some(Compat::Cadvisor),
            _ => None,
        }
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
    /// Measure each container's writable layer, one container at a time.
    pub writable_layer: bool,
    /// Time for one pass over every container.
    #[serde(with = "dur")]
    pub writable_layer_interval: Duration,
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
    /// Gzip request bodies to VictoriaLogs.
    pub compress: bool,
    /// Docker label holding a numeric VictoriaLogs AccountID per container; empty sends everything to tenant 0.
    pub tenant_label: String,
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
            live_interval: Duration::from_secs(5),
            log_level: "info".into(),
            paths: Paths::default(),
            host_metrics: HostMetrics::default(),
            containers: Containers::default(),
            gpu: Gpu::default(),
            logs: Logs::default(),
            remote_write: RemoteWrite::default(),
            api: Api::default(),
            metrics: Metrics::default(),
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
            writable_layer: false,
            writable_layer_interval: Duration::from_secs(3600),
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
            compress: true,
            tenant_label: String::new(),
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
        let mut cfg = from_table(table)?;
        if cfg.host.is_empty() {
            cfg.host = detect_hostname(&cfg.paths.rootfs);
        }
        if !cfg.api.token_file.as_os_str().is_empty() {
            let p = &cfg.api.token_file;
            cfg.api.token = std::fs::read_to_string(p).map_err(|e| format!("token_file {}: {e}", p.display()))?.trim().to_string();
            if cfg.api.token.is_empty() {
                return Err(format!("token_file {}: empty", p.display()));
            }
        }
        if cfg.remote_write.labels.is_empty() {
            cfg.remote_write.labels.insert("host".into(), cfg.host.clone());
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        let intervals = [("interval", self.interval), ("live_interval", self.live_interval), ("logs.poll", self.logs.poll), ("remote_write.interval", self.remote_write.interval)];
        for (name, d) in intervals {
            if d.is_zero() {
                return Err(format!("{name} must be greater than 0"));
            }
        }
        match (self.api.auth, self.api.token.is_empty()) {
            (Auth::Token, true) => Err("api.auth = \"token\" needs api.token or api.token_file".into()),
            (Auth::Off, false) => Err("api.auth = \"off\" conflicts with the token set; remove one".into()),
            _ => Ok(()),
        }
    }

    /// Whether to warn that the API is open: no token, a non-loopback address and `auth` not `off`.
    pub fn warn_open(&self, bound: Option<std::net::SocketAddr>) -> bool {
        self.api.token.is_empty() && self.api.auth != Auth::Off && bound.is_some_and(|a| !a.ip().is_loopback())
    }
}

/// Settings that no longer exist, with the version that removed them.
const REMOVED: &[(&str, &str, &str)] = &[("logs", "multiline", "0.4.0"), ("logs", "api_queue_lines", "0.4.0")];

fn from_table(table: toml::Table) -> Result<Config, String> {
    for (section, key, version) in REMOVED {
        if table.get(*section).and_then(|s| s.get(*key)).is_some() {
            return Err(format!("{section}.{key} was removed in {version}; see CHANGELOG"));
        }
    }
    toml::Value::Table(table).try_into().map_err(|e| format!("config: {e}"))
}

/// Maps COLLECTOR_SECTION__KEY=value onto [section] key = value.
pub fn apply_env(table: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    for (k, v) in vars {
        let Some(rest) = k.strip_prefix(ENV_PREFIX) else { continue };
        if rest == "CONFIG" || rest == "VERSION" {
            continue;
        }
        let parts: Vec<String> = rest.split("__").map(|p| p.to_ascii_lowercase()).collect();
        let value = env_value(&parts, &v);
        set(table, &parts, value);
    }
}

fn set(table: &mut toml::Table, parts: &[String], value: toml::Value) {
    let mut cur = table;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            cur.insert(part.clone(), value);
            return;
        }
        let entry = cur.entry(part.clone()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        cur = entry.as_table_mut().unwrap();
    }
}

/// The raw string when the key accepts one, else the value parsed as TOML.
fn env_value(parts: &[String], v: &str) -> toml::Value {
    let raw = toml::Value::String(v.into());
    let mut probe = toml::Table::new();
    set(&mut probe, parts, raw.clone());
    if toml::Value::Table(probe).try_into::<Config>().is_ok() {
        return raw;
    }
    format!("v = {v}").parse::<toml::Table>().ok().and_then(|mut t| t.remove("v")).unwrap_or(raw)
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
    secs_to_duration(secs).map_err(|e| format!("{e}: {s:?}"))
}

/// Longest duration any setting accepts.
const MAX_DURATION: Duration = Duration::from_secs(366 * 86400);

fn secs_to_duration(secs: f64) -> Result<Duration, String> {
    match Duration::try_from_secs_f64(secs) {
        Ok(d) if d <= MAX_DURATION => Ok(d),
        Ok(_) => Err("duration over 366 days".into()),
        Err(_) => Err("bad duration".into()),
    }
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
            F(f64),
        }
        match Raw::deserialize(d)? {
            Raw::S(s) => super::parse_duration(&s),
            Raw::N(n) => super::secs_to_duration(n as f64),
            Raw::F(f) => super::secs_to_duration(f),
        }
        .map_err(serde::de::Error::custom)
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
        assert_eq!(parse_duration("0.5").unwrap(), Duration::from_millis(500));
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration("9000h").is_err());
        let file = |s: &str| s.parse::<toml::Table>().map_err(|e| e.to_string()).and_then(|t| toml::Value::Table(t).try_into::<Config>().map_err(|e| e.to_string()));
        assert_eq!(file("interval = 0.25").unwrap().interval, Duration::from_millis(250));
        assert!(file("interval = 9223372036854775807").is_err());
        assert!(file("interval = -1").is_err());
    }

    fn env_config(vars: &[(&str, &str)]) -> Result<Config, String> {
        let mut t = toml::Table::new();
        apply_env(&mut t, vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let cfg = from_table(t)?;
        cfg.validate().map(|_| cfg)
    }

    #[test]
    fn env_keeps_strings_raw() {
        let cfg = env_config(&[
            ("COLLECTOR_API__TOKEN", "123456"),
            ("COLLECTOR_HOST", "'abc'"),
            ("COLLECTOR_INTERVAL", "0.5"),
            ("COLLECTOR_LIVE_INTERVAL", "2"),
            ("COLLECTOR_LOGS__BATCH_LINES", "100"),
            ("COLLECTOR_LOGS__COMPRESS", "false"),
            ("COLLECTOR_LOGS__EXCLUDE_NAMES", r#"["^a", "^b"]"#),
            ("COLLECTOR_REMOTE_WRITE__LABELS__DC", "1"),
            ("COLLECTOR_GPU__ENABLED", "off"),
            ("COLLECTOR_VERSION", "1.2.3"),
        ])
        .unwrap();
        assert_eq!(cfg.api.token, "123456");
        assert_eq!(cfg.host, "'abc'");
        assert_eq!(cfg.interval, Duration::from_millis(500));
        assert_eq!(cfg.live_interval, Duration::from_secs(2));
        assert_eq!(cfg.logs.batch_lines, 100);
        assert!(!cfg.logs.compress);
        assert_eq!(cfg.logs.exclude_names, ["^a", "^b"]);
        assert_eq!(cfg.remote_write.labels["dc"], "1");
        assert_eq!(cfg.gpu.enabled, Toggle::Off);
    }

    #[test]
    fn validates_intervals_and_auth() {
        assert!(env_config(&[]).is_ok());
        assert!(env_config(&[("COLLECTOR_INTERVAL", "0")]).unwrap_err().contains("interval must be greater than 0"));
        assert!(env_config(&[("COLLECTOR_LIVE_INTERVAL", "0s")]).is_err());
        assert!(env_config(&[("COLLECTOR_INTERVAL", "100000h")]).is_err());
        assert!(env_config(&[("COLLECTOR_LOGS__POLL", "0")]).unwrap_err().contains("logs.poll must be greater than 0"));
        assert!(env_config(&[("COLLECTOR_REMOTE_WRITE__INTERVAL", "0s")]).unwrap_err().contains("remote_write.interval must be greater than 0"));
        assert!(env_config(&[("COLLECTOR_API__AUTH", "token")]).is_err());
        assert!(env_config(&[("COLLECTOR_API__AUTH", "token"), ("COLLECTOR_API__TOKEN", "t")]).is_ok());
        assert!(env_config(&[("COLLECTOR_API__AUTH", "off"), ("COLLECTOR_API__TOKEN", "t")]).is_err());
        assert!(env_config(&[("COLLECTOR_API__AUTH", "nope")]).is_err());
    }

    #[test]
    fn warns_when_open_beyond_loopback() {
        let any: std::net::SocketAddr = "0.0.0.0:9273".parse().unwrap();
        let lo: std::net::SocketAddr = "127.0.0.1:9273".parse().unwrap();
        let open = env_config(&[]).unwrap();
        assert!(open.warn_open(Some(any)));
        assert!(!open.warn_open(Some(lo)));
        assert!(!open.warn_open(Some("[::1]:9273".parse().unwrap())));
        assert!(!env_config(&[("COLLECTOR_API__AUTH", "off")]).unwrap().warn_open(Some(any)));
        assert!(!env_config(&[("COLLECTOR_API__TOKEN", "t")]).unwrap().warn_open(Some(any)));
    }

    #[test]
    fn empty_token_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("collector-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("c.toml");
        let load = |token: &str| {
            std::fs::write(dir.join("token"), token).unwrap();
            std::fs::write(&conf, format!("[api]\ntoken_file = {:?}\n", dir.join("token").display().to_string())).unwrap();
            Config::load(conf.to_str())
        };
        assert!(load(" \n").unwrap_err().contains("empty"));
        assert_eq!(load("secret\n").unwrap().api.token, "secret");
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn removed_keys_name_the_version() {
        let file = |s: &str| from_table(s.parse().unwrap()).unwrap_err();
        assert_eq!(file("[logs.multiline]\nmode = \"indent\"\n"), "logs.multiline was removed in 0.4.0; see CHANGELOG");
        assert_eq!(file("[logs]\napi_queue_lines = 10\n"), "logs.api_queue_lines was removed in 0.4.0; see CHANGELOG");
        assert_eq!(env_config(&[("COLLECTOR_LOGS__MULTILINE__MODE", "off")]).unwrap_err(), "logs.multiline was removed in 0.4.0; see CHANGELOG");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../collector.example.toml");
        let t: toml::Table = text.parse().unwrap();
        toml::Value::Table(t).try_into::<Config>().unwrap();
    }
}
