use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Line {
    pub log: String,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub time: String,
}

pub fn parse_line(raw: &[u8]) -> Option<Line> {
    serde_json::from_slice(raw).ok()
}

/// Normalizes a log level the way the previous shipper did.
pub fn normalize_level(v: &serde_json::Value) -> Option<&'static str> {
    let s = match v {
        serde_json::Value::String(s) => s.to_ascii_lowercase(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(match s.as_str() {
        "trace" | "10" => "trace",
        "debug" | "20" => "debug",
        "info" | "30" => "info",
        "notice" => "notice",
        "warn" | "warning" | "40" => "warning",
        "error" | "err" | "50" => "error",
        "critical" | "fatal" | "crit" | "60" => "critical",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_json_line() {
        let l = parse_line(br#"{"log":"hello\n","stream":"stderr","time":"2026-10-10T12:00:00.123456789Z"}"#).unwrap();
        assert_eq!(l.log, "hello\n");
        assert_eq!(l.stream, "stderr");
        assert!(parse_line(b"not json").is_none());
    }

    #[test]
    fn levels() {
        use serde_json::json;
        assert_eq!(normalize_level(&json!("WARN")), Some("warning"));
        assert_eq!(normalize_level(&json!(50)), Some("error"));
        assert_eq!(normalize_level(&json!("fatal")), Some("critical"));
        assert_eq!(normalize_level(&json!("verbose")), None);
        assert_eq!(normalize_level(&json!(true)), None);
    }
}
