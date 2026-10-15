use crate::config::{LogSink, Logs};
use std::io::Write;

pub trait Sink: Send {
    fn send(&mut self, ndjson: &[u8]) -> Result<(), String>;
}

pub fn build(cfg: &Logs) -> Result<Box<dyn Sink>, String> {
    match cfg.sink {
        LogSink::Victorialogs => Ok(Box::new(VictoriaLogs::new(cfg))),
        LogSink::File => {
            if let Some(dir) = cfg.file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            let f = std::fs::OpenOptions::new().create(true).append(true).open(&cfg.file).map_err(|e| format!("{}: {e}", cfg.file.display()))?;
            Ok(Box::new(FileSink { f }))
        }
    }
}

pub struct VictoriaLogs {
    agent: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    compress: bool,
}

impl VictoriaLogs {
    pub fn new(cfg: &Logs) -> Self {
        let url = format!(
            "{}/insert/jsonline?_stream_fields={}&_time_field=_time&_msg_field=_msg",
            cfg.url.trim_end_matches('/'),
            cfg.stream_fields.join(",")
        );
        let agent = ureq::AgentBuilder::new().timeout(cfg.timeout).build();
        Self { agent, url, headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), compress: cfg.compress }
    }
}

impl Sink for VictoriaLogs {
    fn send(&mut self, body: &[u8]) -> Result<(), String> {
        let mut req = self.agent.post(&self.url).set("Content-Type", "application/stream+json");
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        let gz;
        let body = if self.compress {
            req = req.set("Content-Encoding", "gzip");
            gz = gzip(body).map_err(|e| format!("gzip: {e}"))?;
            &gz[..]
        } else {
            body
        };
        match req.send_bytes(body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, r)) => Err(format!("victorialogs: HTTP {code}: {}", r.into_string().unwrap_or_default().trim())),
            Err(e) => Err(format!("victorialogs: {e}")),
        }
    }
}

pub struct FileSink {
    f: std::fs::File,
}

impl Sink for FileSink {
    fn send(&mut self, body: &[u8]) -> Result<(), String> {
        self.f.write_all(body).and_then(|_| self.f.flush()).map_err(|e| format!("log file: {e}"))
    }
}

pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 4), flate2::Compression::fast());
    enc.write_all(data)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn gzip_round_trips() {
        let data = b"{\"_msg\":\"hello\"}\n".repeat(100);
        let z = gzip(&data).unwrap();
        assert!(z.len() < data.len() / 5);
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&z[..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
    }
}
