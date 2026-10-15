use crate::config::Compat;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

/// Labels shared by many samples, e.g. one container's identity, in both naming modes.
pub type LabelSet = Arc<Base>;

#[derive(Debug, Default)]
pub struct Base {
    pub clean: Vec<(String, String)>,
    pub cadvisor: Vec<(String, String)>,
}

impl Base {
    fn get(&self, mode: Compat) -> &[(String, String)] {
        match mode {
            Compat::None => &self.clean,
            Compat::Cadvisor => &self.cadvisor,
        }
    }
}

/// Clean metric names that cAdvisor spells differently.
const CADVISOR_NAMES: [(&str, &str); 5] = [
    ("container_memory_rss_bytes", "container_memory_rss"),
    ("container_memory_cache_bytes", "container_memory_cache"),
    ("container_memory_mapped_file_bytes", "container_memory_mapped_file"),
    ("container_memory_kernel_usage_bytes", "container_memory_kernel_usage"),
    ("container_memory_swap_bytes", "container_memory_swap"),
];

/// The name a family renders under in `mode`.
pub fn family_name(name: &str, mode: Compat) -> &str {
    match mode {
        Compat::None => name,
        Compat::Cadvisor => CADVISOR_NAMES.iter().find(|(c, _)| *c == name).map_or(name, |(_, a)| a),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Counter,
    Gauge,
    Untyped,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Untyped => "untyped",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub base: Option<LabelSet>,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn all_labels(&self, mode: Compat) -> impl Iterator<Item = &(String, String)> {
        self.base.iter().flat_map(move |b| b.get(mode).iter()).chain(self.labels.iter())
    }

    fn has_labels(&self, mode: Compat) -> bool {
        !self.labels.is_empty() || self.base.as_ref().is_some_and(|b| !b.get(mode).is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct Family {
    pub name: String,
    pub help: &'static str,
    pub kind: Kind,
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Default)]
pub struct MetricSet {
    pub families: Vec<Family>,
    index: HashMap<String, usize>,
}

pub type Labels<'a> = &'a [(&'a str, &'a str)];

impl MetricSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn family(&mut self, name: &str, help: &'static str, kind: Kind) -> &mut Family {
        if let Some(&i) = self.index.get(name) {
            return &mut self.families[i];
        }
        self.index.insert(name.to_string(), self.families.len());
        self.families.push(Family { name: name.to_string(), help, kind, samples: Vec::new() });
        self.families.last_mut().unwrap()
    }

    pub fn push(&mut self, name: &str, help: &'static str, kind: Kind, labels: Labels, value: f64) {
        let labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.family(name, help, kind).samples.push(Sample { base: None, labels, value });
    }

    /// Pushes a sample whose labels are `base` followed by `extra`.
    pub fn push_with(&mut self, name: &str, help: &'static str, kind: Kind, base: &LabelSet, extra: Labels, value: f64) {
        let labels = extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.family(name, help, kind).samples.push(Sample { base: Some(base.clone()), labels, value });
    }

    pub fn gauge(&mut self, name: &str, help: &'static str, labels: Labels, value: f64) {
        self.push(name, help, Kind::Gauge, labels, value);
    }

    pub fn counter(&mut self, name: &str, help: &'static str, labels: Labels, value: f64) {
        self.push(name, help, Kind::Counter, labels, value);
    }

    pub fn len(&self) -> usize {
        self.families.iter().map(|f| f.samples.len()).sum()
    }

    pub fn render(&self, extra: &[(String, String)], mode: Compat) -> String {
        let mut out = String::with_capacity(self.len() * 96);
        for f in &self.families {
            if f.samples.is_empty() {
                continue;
            }
            let name = family_name(&f.name, mode);
            write_header(&mut out, f, name);
            for s in &f.samples {
                write_sample(&mut out, name, s, extra, mode);
            }
        }
        out
    }
}

fn write_header(out: &mut String, f: &Family, name: &str) {
    let _ = writeln!(out, "# HELP {} {}", name, f.help);
    let _ = writeln!(out, "# TYPE {} {}", name, f.kind.as_str());
}

fn write_sample(out: &mut String, name: &str, s: &Sample, extra: &[(String, String)], mode: Compat) {
    out.push_str(name);
    if s.has_labels(mode) || !extra.is_empty() {
        out.push('{');
        let mut first = true;
        for (k, v) in extra.iter().chain(s.all_labels(mode)) {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(k);
            out.push_str("=\"");
            escape_into(out, v);
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    push_value(out, s.value);
    out.push('\n');
}

/// Renders a MetricSet in small pieces so a scrape never holds the whole text.
pub struct TextReader {
    set: Arc<MetricSet>,
    mode: Compat,
    family: usize,
    sample: usize,
    buf: String,
    pos: usize,
}

impl TextReader {
    pub fn new(set: Arc<MetricSet>, mode: Compat) -> Self {
        Self { set, mode, family: 0, sample: 0, buf: String::with_capacity(32 << 10), pos: 0 }
    }

    fn refill(&mut self) {
        self.buf.clear();
        self.pos = 0;
        while self.buf.len() < 16 << 10 {
            let Some(f) = self.set.families.get(self.family) else { return };
            let name = family_name(&f.name, self.mode);
            if self.sample == 0 && !f.samples.is_empty() {
                write_header(&mut self.buf, f, name);
            }
            match f.samples.get(self.sample) {
                Some(s) => {
                    write_sample(&mut self.buf, name, s, &[], self.mode);
                    self.sample += 1;
                }
                None => {
                    self.family += 1;
                    self.sample = 0;
                }
            }
        }
    }
}

impl std::io::Read for TextReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            self.refill();
        }
        let src = &self.buf.as_bytes()[self.pos..];
        let n = src.len().min(out.len());
        out[..n].copy_from_slice(&src[..n]);
        self.pos += n;
        Ok(n)
    }
}

fn escape_into(out: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn push_value(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "+Inf" } else { "-Inf" });
    } else {
        let _ = write!(out, "{}", v);
    }
}

pub fn sanitize_label_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_format() {
        let mut m = MetricSet::new();
        m.counter("x_total", "Things.", &[("a", "q\"uote\\")], 3.0);
        m.counter("x_total", "Things.", &[("a", "b")], 1.5);
        m.gauge("y", "Y.", &[], f64::NAN);
        let base: LabelSet = Arc::new(Base { clean: vec![("name".to_string(), "web".to_string())], cadvisor: vec![] });
        m.push_with("z", "Z.", Kind::Gauge, &base, &[("device", "sda")], 2.0);
        let z = m.render(&[], Compat::None);
        assert!(z.ends_with("z{name=\"web\",device=\"sda\"} 2\n"));
        m.families.pop();
        m.index.remove("z");
        let out = m.render(&[("host".into(), "h1".into())], Compat::None);
        assert_eq!(
            out,
            "# HELP x_total Things.\n# TYPE x_total counter\nx_total{host=\"h1\",a=\"q\\\"uote\\\\\"} 3\nx_total{host=\"h1\",a=\"b\"} 1.5\n# HELP y Y.\n# TYPE y gauge\ny{host=\"h1\"} NaN\n"
        );
    }

    #[test]
    fn reader_matches_render() {
        use std::io::Read;
        let mut m = MetricSet::new();
        for i in 0..3000 {
            m.counter("a_total", "A.", &[("i", &i.to_string())], i as f64);
        }
        m.gauge("b", "B.", &[], 1.0);
        let base: LabelSet = Arc::new(Base { clean: vec![("container".into(), "web".into())], cadvisor: vec![("name".into(), "web".into())] });
        m.push_with("container_memory_rss_bytes", "R.", Kind::Gauge, &base, &[], 5.0);
        for mode in [Compat::None, Compat::Cadvisor] {
            let want = m.render(&[], mode);
            let mut got = String::new();
            TextReader::new(Arc::new(m.clone()), mode).read_to_string(&mut got).unwrap();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn cadvisor_mode_renames_families_and_labels() {
        let mut m = MetricSet::new();
        let base: LabelSet = Arc::new(Base {
            clean: vec![("container".into(), "shop-web".into()), ("container_id".into(), "3f2a9c4e1b7d".into()), ("compose_project".into(), "shop".into())],
            cadvisor: vec![("id".into(), "/system.slice/docker-3f2a.scope".into()), ("name".into(), "shop-web".into()), ("container_label_com_docker_compose_project".into(), "shop".into())],
        });
        m.push_with("container_memory_rss_bytes", "RSS.", Kind::Gauge, &base, &[], 7.0);
        m.push_with("container_memory_working_set_bytes", "WS.", Kind::Gauge, &base, &[], 9.0);
        m.gauge("node_load1", "Load.", &[], 0.5);
        let clean = m.render(&[], Compat::None);
        assert!(clean.contains("# TYPE container_memory_rss_bytes gauge\ncontainer_memory_rss_bytes{container=\"shop-web\",container_id=\"3f2a9c4e1b7d\",compose_project=\"shop\"} 7\n"));
        let old = m.render(&[], Compat::Cadvisor);
        assert!(old.contains("# TYPE container_memory_rss gauge\ncontainer_memory_rss{id=\"/system.slice/docker-3f2a.scope\",name=\"shop-web\",container_label_com_docker_compose_project=\"shop\"} 7\n"));
        assert!(old.contains("container_memory_working_set_bytes{id="));
        assert!(old.contains("node_load1 0.5\n") && clean.contains("node_load1 0.5\n"));
        assert!(!old.contains("container_memory_rss_bytes") && !old.contains(",compose_project=") && !old.contains("{container=") && !old.contains("container_id="));
    }

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize_label_name("com.docker.compose.project"), "com_docker_compose_project");
        assert_eq!(sanitize_label_name("9x-y"), "_9x_y");
    }
}
