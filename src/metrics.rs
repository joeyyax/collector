use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

/// Labels shared by many samples, e.g. one container's identity.
pub type LabelSet = Arc<[(String, String)]>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Counter,
    Gauge,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
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
    pub fn all_labels(&self) -> impl Iterator<Item = &(String, String)> {
        self.base.iter().flat_map(|b| b.iter()).chain(self.labels.iter())
    }

    fn has_labels(&self) -> bool {
        !self.labels.is_empty() || self.base.as_ref().is_some_and(|b| !b.is_empty())
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

    pub fn render(&self, extra: &[(String, String)]) -> String {
        let mut out = String::with_capacity(self.len() * 96);
        for f in &self.families {
            if f.samples.is_empty() {
                continue;
            }
            write_header(&mut out, f);
            for s in &f.samples {
                write_sample(&mut out, &f.name, s, extra);
            }
        }
        out
    }
}

fn write_header(out: &mut String, f: &Family) {
    let _ = writeln!(out, "# HELP {} {}", f.name, f.help);
    let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind.as_str());
}

fn write_sample(out: &mut String, name: &str, s: &Sample, extra: &[(String, String)]) {
    out.push_str(name);
    if s.has_labels() || !extra.is_empty() {
        out.push('{');
        let mut first = true;
        for (k, v) in extra.iter().chain(s.all_labels()) {
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
    family: usize,
    sample: usize,
    buf: String,
    pos: usize,
}

impl TextReader {
    pub fn new(set: Arc<MetricSet>) -> Self {
        Self { set, family: 0, sample: 0, buf: String::with_capacity(32 << 10), pos: 0 }
    }

    fn refill(&mut self) {
        self.buf.clear();
        self.pos = 0;
        while self.buf.len() < 16 << 10 {
            let Some(f) = self.set.families.get(self.family) else { return };
            if self.sample == 0 && !f.samples.is_empty() {
                write_header(&mut self.buf, f);
            }
            match f.samples.get(self.sample) {
                Some(s) => {
                    write_sample(&mut self.buf, &f.name, s, &[]);
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
        let base: LabelSet = vec![("name".to_string(), "web".to_string())].into();
        m.push_with("z", "Z.", Kind::Gauge, &base, &[("device", "sda")], 2.0);
        let z = m.render(&[]);
        assert!(z.ends_with("z{name=\"web\",device=\"sda\"} 2\n"));
        m.families.pop();
        m.index.remove("z");
        let out = m.render(&[("host".into(), "h1".into())]);
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
        let want = m.render(&[]);
        let mut got = String::new();
        TextReader::new(Arc::new(m)).read_to_string(&mut got).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize_label_name("com.docker.compose.project"), "com_docker_compose_project");
        assert_eq!(sanitize_label_name("9x-y"), "_9x_y");
    }
}
