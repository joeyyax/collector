use crate::config::{Multiline, MultilineMode};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A finished log record: one or more source lines joined.
#[derive(Debug, Clone, PartialEq)]
pub struct Record<P> {
    pub stream: String,
    pub time: String,
    pub msg: String,
    pub start: P,
    pub end: P,
}

#[derive(Debug)]
struct Pending<P> {
    rec: Record<P>,
    lines: usize,
    seq: u64,
    touched: Instant,
}

pub struct Rules {
    mode: MultilineMode,
    pattern: Option<regex_lite::Regex>,
    max_lines: usize,
    max_bytes: usize,
    timeout: Duration,
}

impl Rules {
    pub fn new(cfg: &Multiline) -> Self {
        let pattern = match cfg.mode {
            MultilineMode::Continue | MultilineMode::Start => crate::util::regex(&cfg.pattern),
            _ => None,
        };
        Self { mode: cfg.mode, pattern, max_lines: cfg.max_lines.max(1), max_bytes: cfg.max_bytes.max(1024), timeout: cfg.timeout }
    }

    #[cfg(test)]
    pub fn off() -> Self {
        Self { mode: MultilineMode::Off, pattern: None, max_lines: 1, max_bytes: 1 << 20, timeout: Duration::ZERO }
    }

    fn continues(&self, line: &str) -> bool {
        match self.mode {
            MultilineMode::Off => false,
            MultilineMode::Indent => line.starts_with(' ') || line.starts_with('\t'),
            MultilineMode::Continue => self.pattern.as_ref().is_some_and(|r| r.is_match(line)),
            MultilineMode::Start => self.pattern.as_ref().is_some_and(|r| !r.is_match(line)),
        }
    }
}

/// One source line as read.
pub struct Piece<'a, P> {
    pub stream: &'a str,
    pub time: &'a str,
    /// Keeps its trailing newline; a missing one marks a partial piece.
    pub text: &'a str,
    pub start: P,
    pub end: P,
}

/// Rejoins Docker's 16 KB partial messages, then groups multi-line records, per stream.
pub struct Assembler<P> {
    partial: HashMap<String, Pending<P>>,
    group: HashMap<String, Pending<P>>,
    seq: u64,
}

/// Docker splits messages longer than this into partial pieces; joined messages are capped well above it.
const MAX_PARTIAL_BYTES: usize = 1 << 20;

impl<P: Copy> Default for Assembler<P> {
    fn default() -> Self {
        Self { partial: HashMap::new(), group: HashMap::new(), seq: 0 }
    }
}

impl<P: Copy> Assembler<P> {
    /// Feeds one source line.
    pub fn push(&mut self, rules: &Rules, piece: Piece<'_, P>, out: &mut Vec<Record<P>>) {
        let Piece { stream, time, text, start, end } = piece;
        let partial = !text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let now = Instant::now();
        self.seq += 1;
        let (mut line, seq) = match self.partial.remove(stream) {
            Some(mut p) => {
                p.rec.msg.push_str(body);
                p.rec.end = end;
                if partial && p.rec.msg.len() < MAX_PARTIAL_BYTES {
                    p.touched = now;
                    self.partial.insert(stream.to_string(), p);
                    return;
                }
                (p.rec, p.seq)
            }
            None if partial => {
                let rec = Record { stream: stream.into(), time: time.into(), msg: body.into(), start, end };
                self.partial.insert(stream.to_string(), Pending { rec, lines: 1, seq: self.seq, touched: now });
                return;
            }
            None => (Record { stream: stream.into(), time: time.into(), msg: body.into(), start, end }, self.seq),
        };
        if line.msg.ends_with('\r') {
            line.msg.pop();
        }
        if rules.mode == MultilineMode::Off {
            out.push(line);
            return;
        }
        if let Some(g) = self.group.get_mut(stream) {
            if rules.continues(&line.msg) && g.lines < rules.max_lines && g.rec.msg.len() + line.msg.len() < rules.max_bytes {
                g.rec.msg.push('\n');
                g.rec.msg.push_str(&line.msg);
                g.rec.end = line.end;
                g.lines += 1;
                g.touched = now;
                return;
            }
        }
        if let Some(g) = self.group.remove(stream) {
            out.push(g.rec);
        }
        self.group.insert(stream.to_string(), Pending { rec: line, lines: 1, seq, touched: now });
    }

    /// Emits groups that saw no new line within the timeout.
    pub fn flush_idle(&mut self, rules: &Rules, out: &mut Vec<Record<P>>) {
        let idle: Vec<String> = self.group.iter().filter(|(_, g)| g.touched.elapsed() >= rules.timeout).map(|(k, _)| k.clone()).collect();
        for k in idle {
            if let Some(g) = self.group.remove(&k) {
                out.push(g.rec);
            }
        }
    }

    /// Emits every multi-line group; partial pieces stay until their last piece arrives.
    pub fn flush_groups(&mut self, out: &mut Vec<Record<P>>) {
        let mut gs: Vec<_> = self.group.drain().map(|(_, g)| g).collect();
        gs.sort_by_key(|g| g.seq);
        out.extend(gs.into_iter().map(|g| g.rec));
    }

    /// Emits everything, partial pieces included, e.g. when a container is gone.
    pub fn flush_all(&mut self, out: &mut Vec<Record<P>>) {
        let mut ps: Vec<_> = self.partial.drain().map(|(_, p)| p).chain(self.group.drain().map(|(_, g)| g)).collect();
        ps.sort_by_key(|p| p.seq);
        out.extend(ps.into_iter().map(|p| p.rec));
    }

    /// Start of the oldest record still held back.
    pub fn oldest_start(&self) -> Option<P> {
        self.partial.values().chain(self.group.values()).min_by_key(|p| p.seq).map(|p| p.rec.start)
    }

    pub fn is_empty(&self) -> bool {
        self.partial.is_empty() && self.group.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(mode: MultilineMode, pattern: &str) -> Rules {
        Rules::new(&Multiline { mode, pattern: pattern.into(), max_lines: 100, max_bytes: 1 << 20, timeout: Duration::ZERO })
    }

    fn feed(a: &mut Assembler<u32>, r: &Rules, lines: &[(&str, &str)]) -> Vec<Record<u32>> {
        let mut out = Vec::new();
        for (i, (s, t)) in lines.iter().enumerate() {
            a.push(r, Piece { stream: s, time: &format!("t{i}"), text: t, start: i as u32, end: i as u32 + 1 }, &mut out);
        }
        out
    }

    #[test]
    fn joins_partial_pieces() {
        let mut a = Assembler::default();
        let r = Rules::off();
        let out = feed(&mut a, &r, &[("stdout", "aaa"), ("stderr", "x\n"), ("stdout", "bbb"), ("stdout", "ccc\n")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].msg, "x");
        assert_eq!(out[1].msg, "aaabbbccc");
        assert_eq!(out[1].time, "t0");
        assert_eq!((out[1].start, out[1].end), (0, 4));
        assert!(a.is_empty());
    }

    #[test]
    fn holds_partial_and_reports_oldest() {
        let mut a = Assembler::default();
        let r = Rules::off();
        let out = feed(&mut a, &r, &[("stdout", "x\n"), ("stdout", "half")]);
        assert_eq!(out.len(), 1);
        assert_eq!(a.oldest_start(), Some(1));
        let mut rest = Vec::new();
        a.flush_groups(&mut rest);
        assert!(rest.is_empty());
        a.flush_all(&mut rest);
        assert_eq!(rest[0].msg, "half");
    }

    #[test]
    fn groups_indented_lines() {
        let mut a = Assembler::default();
        let r = rules(MultilineMode::Indent, "");
        let mut out = feed(&mut a, &r, &[("stderr", "Traceback:\n"), ("stderr", "  File x\n"), ("stderr", "\tline 2\n"), ("stderr", "Error: boom\n")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg, "Traceback:\n  File x\n\tline 2");
        assert_eq!(a.oldest_start(), Some(3));
        a.flush_idle(&r, &mut out);
        assert_eq!(out[1].msg, "Error: boom");
        assert!(a.is_empty());
    }

    #[test]
    fn start_pattern_begins_records() {
        let mut a = Assembler::default();
        let r = rules(MultilineMode::Start, r"^\d{4}-");
        let mut out = feed(&mut a, &r, &[("stdout", "2026-01-01 one\n"), ("stdout", "detail\n"), ("stdout", "2026-01-01 two\n")]);
        a.flush_groups(&mut out);
        assert_eq!(out.iter().map(|r| r.msg.as_str()).collect::<Vec<_>>(), vec!["2026-01-01 one\ndetail", "2026-01-01 two"]);
    }

    #[test]
    fn caps_group_size() {
        let mut a = Assembler::default();
        let r = Rules::new(&Multiline { mode: MultilineMode::Indent, pattern: String::new(), max_lines: 2, max_bytes: 1 << 20, timeout: Duration::ZERO });
        let mut out = feed(&mut a, &r, &[("o", "a\n"), ("o", " b\n"), ("o", " c\n")]);
        a.flush_groups(&mut out);
        assert_eq!(out.iter().map(|r| r.msg.as_str()).collect::<Vec<_>>(), vec!["a\n b", " c"]);
    }

    #[test]
    fn strips_carriage_return() {
        let mut a = Assembler::default();
        let out = feed(&mut a, &Rules::off(), &[("o", "dos\r\n")]);
        assert_eq!(out[0].msg, "dos");
    }
}
