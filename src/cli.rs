use crate::config::Config;
use serde_json::Value;
use std::time::Duration;

pub const COMMANDS: [&str; 4] = ["health", "host", "containers", "gpus"];

pub const USAGE: &str = "usage: collector health|host|containers|gpus [--url URL] [--token-file FILE] [--json]
                  containers also takes [--sort KEY] [--limit N]

  --url URL          collector to ask (default: http://127.0.0.1:<port from listen>)
  --token-file FILE  bearer token for the API (default: api.token or api.token_file)
  --json             print the raw response
  --sort KEY         cpu, memory, net_rx, net_tx, disk_read, disk_write or name
  --limit N          at most N containers

health exits 0 when the collector is fresh, 1 otherwise.";

struct Opts {
    url: Option<String>,
    token_file: Option<String>,
    json: bool,
    sort: Option<String>,
    limit: Option<String>,
}

/// Runs a subcommand and returns the process exit code.
pub fn run(cmd: &str, args: impl Iterator<Item = String>, config_path: Option<&str>) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("collector: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let cfg = Config::load(config_path).ok();
    let base = opts.url.clone().unwrap_or_else(|| default_url(cfg.as_ref().map_or("127.0.0.1:9273", |c| c.listen.as_str())));
    let token = match &opts.token_file {
        Some(f) => match std::fs::read_to_string(f) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!("collector: token file {f}: {e}");
                return 2;
            }
        },
        None => cfg.map(|c| c.api.token).unwrap_or_default(),
    };
    let path = match cmd {
        "health" => "/health".to_string(),
        "containers" => {
            let q: Vec<String> = [("sort", &opts.sort), ("limit", &opts.limit)].iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}"))).collect();
            if q.is_empty() {
                "/api/v1/containers".to_string()
            } else {
                format!("/api/v1/containers?{}", q.join("&"))
            }
        }
        other => format!("/api/v1/{other}"),
    };
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let (status, body) = match get(&url, &token) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("collector: {e}");
            return 1;
        }
    };
    if cmd == "health" {
        if opts.json {
            println!("{body}");
        } else {
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => print!("{}", health(&v)),
                Err(_) => println!("{body}"),
            }
        }
        return if status == 200 { 0 } else { 1 };
    }
    if status != 200 {
        eprintln!("collector: {url}: HTTP {status}: {}", body.trim());
        return 1;
    }
    if opts.json {
        println!("{body}");
        return 0;
    }
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("collector: {url}: {e}");
            return 1;
        }
    };
    print!(
        "{}",
        match cmd {
            "host" => host(&v),
            "containers" => containers(&v),
            _ => gpus(&v),
        }
    );
    0
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
    let mut o = Opts { url: None, token_file: None, json: false, sort: None, limit: None };
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--url" => o.url = Some(value("--url")?),
            "--token-file" => o.token_file = Some(value("--token-file")?),
            "--sort" => o.sort = Some(value("--sort")?),
            "--limit" => o.limit = Some(value("--limit")?),
            "--json" => o.json = true,
            _ => return Err(format!("unknown argument {a:?}")),
        }
    }
    Ok(o)
}

/// The loopback URL for a `listen` address; wildcard hosts become 127.0.0.1.
pub fn default_url(listen: &str) -> String {
    let port = listen.rsplit_once(':').map_or("9273", |(_, p)| p);
    let host = listen.rsplit_once(':').map_or("", |(h, _)| h);
    let host = match host {
        "" | "0.0.0.0" | "[::]" | "::" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}")
}

fn get(url: &str, token: &str) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
    let mut req = agent.get(url);
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(r) => Ok((r.status(), r.into_string().map_err(|e| e.to_string())?)),
        Err(ureq::Error::Status(code, r)) => Ok((code, r.into_string().unwrap_or_default())),
        Err(e) => Err(e.to_string()),
    }
}

fn health(v: &Value) -> String {
    let mut rows = vec![
        vec!["status".to_string(), str_of(&v["status"])],
        vec!["version".to_string(), str_of(&v["version"])],
        vec!["host".to_string(), str_of(&v["host"])],
        vec!["last collection".to_string(), v["last_collect_age_seconds"].as_f64().map_or("-".into(), |a| format!("{a:.1}s ago"))],
        vec!["interval".to_string(), v["interval_seconds"].as_f64().map_or("-".into(), |i| format!("{i}s"))],
    ];
    if let Some(d) = v["docker"].as_bool() {
        rows.push(vec!["docker".into(), if d { "up" } else { "down" }.into()]);
    }
    if let Some(t) = v["logs_last_success"].as_u64() {
        rows.push(vec!["logs last shipped".into(), if t == 0 { "never".into() } else { t.to_string() }]);
    }
    if let Some(g) = v["gpu_per_container"].as_bool() {
        rows.push(vec!["gpu per container".into(), if g { "on" } else { "off" }.into()]);
    }
    table(&[], &rows)
}

fn host(v: &Value) -> String {
    let mut out = String::new();
    let cpu = &v["cpu"];
    let mem = &v["memory"];
    let load = &v["load"];
    out += &table(
        &[],
        &[
            vec!["host".into(), str_of(&v["host"])],
            vec!["uptime".into(), v["uptime_seconds"].as_f64().map_or("-".into(), duration)],
            vec!["cpu".into(), format!("{} of {} cores", ratio(&cpu["usage_ratio"]), num(&cpu["count"]))],
            vec!["load".into(), format!("{} {} {}", fixed(&load["load1"]), fixed(&load["load5"]), fixed(&load["load15"]))],
            vec!["memory".into(), format!("{} used of {}, {} available", bytes(&mem["used_bytes"]), bytes(&mem["total_bytes"]), bytes(&mem["available_bytes"]))],
            vec!["swap".into(), format!("{} used of {}", bytes(&mem["swap_used_bytes"]), bytes(&mem["swap_total_bytes"]))],
        ],
    );
    let fs: Vec<Vec<String>> = list(&v["filesystems"]).iter().map(|f| vec![str_of(&f["mountpoint"]), str_of(&f["device"]), bytes(&f["size_bytes"]), ratio(&f["used_ratio"])]).collect();
    if !fs.is_empty() {
        out += "\n";
        out += &table(&["MOUNT", "DEVICE", "SIZE", "USED"], &fs);
    }
    let disks: Vec<Vec<String>> = list(&v["disks"]).iter().map(|d| vec![str_of(&d["device"]), rate(&d["read_bytes_per_second"]), rate(&d["written_bytes_per_second"]), ratio(&d["busy_ratio"])]).collect();
    if !disks.is_empty() {
        out += "\n";
        out += &table(&["DISK", "READ", "WRITE", "BUSY"], &disks);
    }
    let nets: Vec<Vec<String>> = list(&v["networks"]).iter().map(|n| vec![str_of(&n["device"]), rate(&n["receive_bytes_per_second"]), rate(&n["transmit_bytes_per_second"])]).collect();
    if !nets.is_empty() {
        out += "\n";
        out += &table(&["NETWORK", "RX", "TX"], &nets);
    }
    out
}

fn containers(v: &Value) -> String {
    let sum = |arr: &Value, key: &str| -> Value {
        let items = list(arr);
        items.iter().try_fold(0.0, |acc, i| i[key].as_f64().map(|v| acc + v)).map_or(Value::Null, Value::from)
    };
    let rows: Vec<Vec<String>> = list(v)
        .iter()
        .map(|c| {
            vec![
                str_of(&c["name"]),
                str_of(&c["lifecycle"]["state"]),
                c["lifecycle"]["health"].as_str().unwrap_or("-").to_string(),
                c["cpu"]["usage_cores"].as_f64().map_or("-".into(), |v| format!("{v:.2}")),
                bytes(&c["memory"]["working_set_bytes"]),
                bytes(&c["memory"]["limit_bytes"]),
                rate(&sum(&c["network"], "receive_bytes_per_second")),
                rate(&sum(&c["network"], "transmit_bytes_per_second")),
                rate(&sum(&c["io"], "read_bytes_per_second")),
                rate(&sum(&c["io"], "write_bytes_per_second")),
                num(&c["lifecycle"]["restarts_total"]),
            ]
        })
        .collect();
    table(&["NAME", "STATE", "HEALTH", "CPU", "MEMORY", "LIMIT", "NET RX", "NET TX", "READ", "WRITE", "RESTARTS"], &rows)
}

fn gpus(v: &Value) -> String {
    let rows: Vec<Vec<String>> = list(v)
        .iter()
        .map(|g| {
            let users = match g["containers"].as_array() {
                None => "-".to_string(),
                Some(a) if a.is_empty() => "none".to_string(),
                Some(a) => a.iter().map(|u| format!("{} {}", str_of(&u["name"]), bytes(&u["memory_used_bytes"]))).collect::<Vec<_>>().join(", "),
            };
            vec![
                num(&g["index"]),
                str_of(&g["name"]),
                ratio(&g["utilization_ratio"]),
                format!("{} / {}", bytes(&g["memory_used_bytes"]), bytes(&g["memory_total_bytes"])),
                g["temperature_celsius"].as_f64().map_or("-".into(), |t| format!("{t:.0}°C")),
                g["power_draw_watts"].as_f64().map_or("-".into(), |w| format!("{w:.0} W")),
                users,
            ]
        })
        .collect();
    table(&["GPU", "NAME", "UTIL", "MEMORY", "TEMP", "POWER", "CONTAINERS"], &rows)
}

/// Left-aligned columns two spaces apart; no header row when `headers` is empty.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let head: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let all: Vec<&Vec<String>> = (!head.is_empty()).then_some(&head).into_iter().chain(rows).collect();
    let cols = all.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols).map(|i| all.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in all {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| if i + 1 == r.len() { c.clone() } else { format!("{c:<w$}", w = widths[i]) }).collect();
        out += line.join("  ").trim_end();
        out.push('\n');
    }
    out
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn num(v: &Value) -> String {
    v.as_u64().map_or("-".into(), |n| n.to_string())
}

fn fixed(v: &Value) -> String {
    v.as_f64().map_or("-".into(), |n| format!("{n:.2}"))
}

fn ratio(v: &Value) -> String {
    v.as_f64().map_or("-".into(), |r| format!("{:.0}%", r * 100.0))
}

fn bytes(v: &Value) -> String {
    v.as_f64().map_or("-".into(), human_bytes)
}

fn rate(v: &Value) -> String {
    v.as_f64().map_or("-".into(), |b| format!("{}/s", human_bytes(b)))
}

pub fn human_bytes(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn duration(secs: f64) -> String {
    let s = secs as u64;
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_url_from_listen() {
        assert_eq!(default_url("0.0.0.0:9273"), "http://127.0.0.1:9273");
        assert_eq!(default_url("127.0.0.1:9300"), "http://127.0.0.1:9300");
        assert_eq!(default_url("[::]:9273"), "http://127.0.0.1:9273");
        assert_eq!(default_url("192.0.2.10:9273"), "http://192.0.2.10:9273");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512.0), "512 B");
        assert_eq!(human_bytes(1536.0), "1.5 KiB");
        assert_eq!(human_bytes(51539607552.0), "48.0 GiB");
    }

    #[test]
    fn containers_table() {
        let v = json!([
            {"name": "shop-redis", "lifecycle": {"state": "running", "health": "healthy", "restarts_total": 2},
             "cpu": {"usage_cores": 0.0096}, "memory": {"working_set_bytes": 119992320, "limit_bytes": 536870912},
             "network": [{"receive_bytes_per_second": 1024.0, "transmit_bytes_per_second": 10.0}, {"receive_bytes_per_second": 1024.0, "transmit_bytes_per_second": null}],
             "io": [{"read_bytes_per_second": 0.0, "write_bytes_per_second": 2048.0}]},
            {"name": "web", "lifecycle": {"state": "running", "health": null, "restarts_total": 0},
             "cpu": {"usage_cores": null}, "memory": {"working_set_bytes": 1000, "limit_bytes": null}, "network": [], "io": []}
        ]);
        let got = containers(&v);
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["NAME", "STATE", "HEALTH", "CPU", "MEMORY", "LIMIT", "NET", "RX", "NET", "TX", "READ", "WRITE", "RESTARTS"]);
        assert_eq!(lines[1], "shop-redis  running  healthy  0.01  114.4 MiB  512.0 MiB  2.0 KiB/s  -       0 B/s  2.0 KiB/s  2");
        assert_eq!(lines[2], "web         running  -        -     1000 B     -          0 B/s      0 B/s   0 B/s  0 B/s      0");
    }

    #[test]
    fn gpus_table_tells_unknown_from_unused() {
        let g = |containers: Value| json!({"index": 0, "name": "RTX 3060", "utilization_ratio": 0.41, "memory_used_bytes": 2147483648.0, "memory_total_bytes": 12884901888.0, "temperature_celsius": 37.0, "power_draw_watts": 14.05, "containers": containers});
        let out = gpus(&json!([g(Value::Null), g(json!([])), g(json!([{"id": "a", "name": "llm", "memory_used_bytes": 1073741824}]))]));
        let last = |l: &str| l.rsplit("  ").next().unwrap().to_string();
        let lines: Vec<String> = out.lines().skip(1).map(last).collect();
        assert_eq!(lines, ["-", "none", "llm 1.0 GiB"]);
        assert!(out.contains("0    RTX 3060  41%   2.0 GiB / 12.0 GiB  37°C  14 W"));
    }

    #[test]
    fn health_lines() {
        let v = json!({"status": "stale", "version": "0.3.0", "host": "web-1", "last_collect_age_seconds": 61.26, "collected_at": 1.0, "interval_seconds": 15.0, "docker": false, "gpu_per_container": true});
        let out = health(&v);
        assert!(out.starts_with("status             stale\n"));
        assert!(out.contains("last collection    61.3s ago\n") && out.contains("docker             down\n") && out.contains("gpu per container  on\n"));
        assert!(!out.contains("logs"));
    }
}
