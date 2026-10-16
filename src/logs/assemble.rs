/// A finished log record: one source line, or Docker's partial pieces joined.
#[derive(Debug, Clone, PartialEq)]
pub struct Record<P> {
    pub stream: String,
    pub time: String,
    pub msg: String,
    pub start: P,
    pub end: P,
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

struct Held<P> {
    rec: Record<P>,
    open: bool,
}

/// Rejoins Docker's 16 KB partial pieces per stream.
/// While any record is unfinished, every later record is held too, so nothing before the oldest held start is pending.
pub struct Assembler<P> {
    held: Vec<Held<P>>,
    bytes: usize,
}

/// Cap on a joined message, and on everything held behind an unfinished one.
const MAX_HELD_BYTES: usize = 1 << 20;

impl<P> Default for Assembler<P> {
    fn default() -> Self {
        Self { held: Vec::new(), bytes: 0 }
    }
}

impl<P: Copy> Assembler<P> {
    /// Feeds one source line; finished records go to `out` in file order.
    pub fn push(&mut self, piece: Piece<'_, P>, out: &mut Vec<Record<P>>) {
        let Piece { stream, time, text, start, end } = piece;
        let partial = !text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        if self.held.is_empty() && !partial {
            out.push(finish(Record { stream: stream.into(), time: time.into(), msg: body.into(), start, end }));
            return;
        }
        self.bytes += body.len();
        match self.held.iter_mut().find(|h| h.open && h.rec.stream == stream) {
            Some(h) => {
                h.rec.msg.push_str(body);
                h.rec.end = end;
                h.open = partial && h.rec.msg.len() < MAX_HELD_BYTES;
            }
            None => self.held.push(Held { rec: Record { stream: stream.into(), time: time.into(), msg: body.into(), start, end }, open: partial }),
        }
        if self.bytes >= MAX_HELD_BYTES || !self.held.iter().any(|h| h.open) {
            self.flush(out);
        }
    }

    /// Emits everything held, unfinished records included.
    pub fn flush(&mut self, out: &mut Vec<Record<P>>) {
        out.extend(self.held.drain(..).map(|h| finish(h.rec)));
        self.bytes = 0;
    }

    /// Start of the oldest record still held back.
    pub fn oldest_start(&self) -> Option<P> {
        self.held.first().map(|h| h.rec.start)
    }
}

fn finish<P>(mut r: Record<P>) -> Record<P> {
    if r.msg.ends_with('\r') {
        r.msg.pop();
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(a: &mut Assembler<u32>, lines: &[(&str, &str)]) -> Vec<Record<u32>> {
        let mut out = Vec::new();
        for (i, (s, t)) in lines.iter().enumerate() {
            a.push(Piece { stream: s, time: &format!("t{i}"), text: t, start: i as u32, end: i as u32 + 1 }, &mut out);
        }
        out
    }

    fn msgs(out: &[Record<u32>]) -> Vec<&str> {
        out.iter().map(|r| r.msg.as_str()).collect()
    }

    #[test]
    fn joins_partial_pieces() {
        let mut a = Assembler::default();
        let out = feed(&mut a, &[("stdout", "aaa"), ("stderr", "x\n"), ("stdout", "bbb"), ("stdout", "ccc\n")]);
        assert_eq!(msgs(&out), ["aaabbbccc", "x"]);
        assert_eq!(out[0].time, "t0");
        assert_eq!((out[0].start, out[0].end), (0, 4));
        assert_eq!(a.oldest_start(), None);
    }

    #[test]
    fn holds_later_records_behind_an_unfinished_one() {
        let mut a = Assembler::default();
        let mut out = feed(&mut a, &[("stdout", "x\n"), ("stdout", "half"), ("stderr", "y\n")]);
        assert_eq!(msgs(&out), ["x"]);
        assert_eq!(a.oldest_start(), Some(1));
        a.flush(&mut out);
        assert_eq!(msgs(&out), ["x", "half", "y"]);
        assert_eq!(a.oldest_start(), None);
    }

    #[test]
    fn interleaved_partials_release_together() {
        let mut a = Assembler::default();
        let out = feed(&mut a, &[("stdout", "a1"), ("stderr", "b1"), ("stdout", "a2\n")]);
        assert!(out.is_empty());
        assert_eq!(a.oldest_start(), Some(0));
        let mut out = Vec::new();
        a.push(Piece { stream: "stderr", time: "t", text: "b2\n", start: 3, end: 4 }, &mut out);
        assert_eq!(msgs(&out), ["a1a2", "b1b2"]);
    }

    #[test]
    fn caps_joined_size() {
        let mut a = Assembler::default();
        let piece = "p".repeat(16 << 10);
        let mut out = Vec::new();
        for i in 0..70 {
            a.push(Piece { stream: "stdout", time: "t", text: &piece, start: i, end: i + 1 }, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg.len(), MAX_HELD_BYTES);
        assert_eq!(a.oldest_start(), Some(64));
    }

    #[test]
    fn strips_carriage_return() {
        let mut a = Assembler::default();
        let out = feed(&mut a, &[("o", "dos\r\n"), ("o", "pa"), ("o", "rt\r\n")]);
        assert_eq!(msgs(&out), ["dos", "part"]);
    }
}
