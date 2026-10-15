use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A byte position in one log file, identified by inode so it survives rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub inode: u64,
    pub offset: u64,
}

/// Where a container's log stands after the last delivered batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// Reading resumes here: the start of the oldest record not yet delivered.
    pub inode: u64,
    pub offset: u64,
    /// Lines before this point were delivered already; replay them only to rebuild pending records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_until: Option<Cursor>,
}

impl Saved {
    pub fn at(c: Cursor) -> Self {
        Self { inode: c.inode, offset: c.offset, replay_until: None }
    }
    pub fn cursor(&self) -> Cursor {
        Cursor { inode: self.inode, offset: self.offset }
    }
}

/// Position in a Docker logs API stream: the next timestamp to read, in nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSaved {
    pub since_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_until_ns: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub cursors: BTreeMap<String, Saved>,
    #[serde(default)]
    pub api: BTreeMap<String, ApiSaved>,
}

impl State {
    pub fn load(path: &Path) -> Option<State> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[derive(Debug, Clone)]
struct LogFile {
    path: PathBuf,
    inode: u64,
    size: u64,
}

/// The current log file and its uncompressed rotations, oldest first.
fn chain(current: &Path) -> Vec<LogFile> {
    let mut files = Vec::new();
    for n in (1..=32).rev() {
        let p = PathBuf::from(format!("{}.{n}", current.display()));
        if let Ok(md) = std::fs::metadata(&p) {
            files.push(LogFile { path: p, inode: md.ino(), size: md.len() });
        }
    }
    if let Ok(md) = std::fs::metadata(current) {
        files.push(LogFile { path: current.to_path_buf(), inode: md.ino(), size: md.len() });
    }
    files
}

pub enum Start {
    Beginning,
    End,
}

/// Where reading starts when a container has no cursor.
pub fn initial(current: &Path, start: Start) -> Option<Cursor> {
    let files = chain(current);
    match start {
        Start::Beginning => files.first().map(|f| Cursor { inode: f.inode, offset: 0 }),
        Start::End => files.last().map(|f| Cursor { inode: f.inode, offset: f.size }),
    }
}

/// Whether `pos` is in the file chain at all.
pub fn reachable(current: &Path, pos: Cursor) -> bool {
    chain(current).iter().any(|f| f.inode == pos.inode)
}

/// One complete line and where it sits.
pub struct Line {
    pub range: std::ops::Range<usize>,
    pub start: Cursor,
    pub end: Cursor,
}

pub struct Chunk {
    pub buf: Vec<u8>,
    pub lines: Vec<Line>,
    pub cursor: Cursor,
    /// The cursor's file had rotated out of reach; lines were lost.
    pub lost: bool,
}

impl Chunk {
    pub fn line(&self, l: &Line) -> &[u8] {
        &self.buf[l.range.clone()]
    }
}

/// Reads up to `budget` bytes of complete lines after `cur`, following rotations.
pub fn read_from(current: &Path, cur: Cursor, budget: usize) -> Option<Chunk> {
    let files = chain(current);
    if files.is_empty() {
        return None;
    }
    let mut lost = false;
    let mut idx = match files.iter().position(|f| f.inode == cur.inode) {
        Some(i) => i,
        None => {
            lost = true;
            0
        }
    };
    let mut cursor = if lost { Cursor { inode: files[0].inode, offset: 0 } } else { cur };
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    loop {
        let f = &files[idx];
        if f.size < cursor.offset {
            cursor.offset = 0;
        }
        let want = (f.size - cursor.offset).min(budget.saturating_sub(buf.len()) as u64) as usize;
        if want > 0 {
            let start = buf.len();
            buf.resize(start + want, 0);
            let mut file = File::open(&f.path).ok()?;
            file.seek(SeekFrom::Start(cursor.offset)).ok()?;
            let n = read_full(&mut file, &mut buf[start..]).ok()?;
            buf.truncate(start + n);
            let at_eof = cursor.offset + n as u64 >= f.size;
            let rotated = idx + 1 < files.len();
            let mut pos = start;
            while let Some(i) = buf[pos..].iter().position(|&b| b == b'\n') {
                let s = Cursor { inode: f.inode, offset: cursor.offset + (pos - start) as u64 };
                let e = Cursor { inode: f.inode, offset: s.offset + i as u64 + 1 };
                lines.push(Line { range: pos..pos + i, start: s, end: e });
                pos += i + 1;
            }
            let complete = pos - start;
            buf.truncate(pos);
            if complete == 0 && start == 0 && n >= budget {
                // A single line longer than the budget: skip it rather than stall.
                cursor.offset += n as u64;
            } else if rotated && at_eof {
                // Rotated files are final; drop any trailing partial line.
                cursor.offset = f.size;
            } else {
                cursor.offset += complete as u64;
            }
        }
        let drained = cursor.offset >= f.size;
        if drained && idx + 1 < files.len() && buf.len() < budget {
            idx += 1;
            cursor = Cursor { inode: files[idx].inode, offset: 0 };
            continue;
        }
        break;
    }
    Some(Chunk { buf, lines, cursor, lost })
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("collector-cur-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn append(p: &Path, s: &str) {
        std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
    }

    fn text(c: &Chunk) -> String {
        c.lines.iter().map(|l| format!("{}\n", String::from_utf8_lossy(c.line(l)))).collect()
    }

    #[test]
    fn reads_complete_lines_and_resumes() {
        let d = dir("resume");
        let log = d.join("c-json.log");
        append(&log, "one\ntwo\nthr");
        let c = initial(&log, Start::Beginning).unwrap();
        let r = read_from(&log, c, 1024).unwrap();
        assert_eq!(text(&r), "one\ntwo\n");
        assert_eq!(r.cursor.offset, 8);
        assert_eq!((r.lines[1].start.offset, r.lines[1].end.offset), (4, 8));
        append(&log, "ee\n");
        let r = read_from(&log, r.cursor, 1024).unwrap();
        assert_eq!(text(&r), "three\n");
        let r2 = read_from(&log, r.cursor, 1024).unwrap();
        assert!(r2.lines.is_empty());
    }

    #[test]
    fn respects_budget() {
        let d = dir("budget");
        let log = d.join("c-json.log");
        append(&log, "aaaa\nbbbb\ncccc\n");
        let c = initial(&log, Start::Beginning).unwrap();
        let r = read_from(&log, c, 11).unwrap();
        assert_eq!(text(&r), "aaaa\nbbbb\n");
        let r = read_from(&log, r.cursor, 11).unwrap();
        assert_eq!(text(&r), "cccc\n");
    }

    #[test]
    fn follows_rotation_without_loss() {
        let d = dir("rotate");
        let log = d.join("c-json.log");
        append(&log, "a\nb\n");
        let c = initial(&log, Start::Beginning).unwrap();
        let r = read_from(&log, c, 2).unwrap();
        assert_eq!(text(&r), "a\n");
        append(&log, "c\n");
        std::fs::rename(&log, d.join("c-json.log.1")).unwrap();
        append(&log, "d\n");
        let r = read_from(&log, r.cursor, 1024).unwrap();
        assert!(!r.lost);
        assert_eq!(text(&r), "b\nc\nd\n");
        assert_ne!(r.lines[1].start.inode, r.lines[2].start.inode);
        assert_eq!(r.lines[2].start.offset, 0);
    }

    #[test]
    fn end_start_skips_history() {
        let d = dir("end");
        let log = d.join("c-json.log");
        append(&log, "old\n");
        let c = initial(&log, Start::End).unwrap();
        append(&log, "new\n");
        assert_eq!(text(&read_from(&log, c, 1024).unwrap()), "new\n");
    }

    #[test]
    fn reports_lost_cursor() {
        let d = dir("lost");
        let log = d.join("c-json.log");
        append(&log, "x\n");
        let r = read_from(&log, Cursor { inode: 1, offset: 5 }, 1024).unwrap();
        assert!(r.lost);
        assert_eq!(text(&r), "x\n");
    }

    #[test]
    fn state_round_trips_and_reads_old_format() {
        let d = dir("state");
        let p = d.join("cursors.json");
        let mut s = State { version: 1, ..Default::default() };
        s.cursors.insert("abc".into(), Saved { inode: 7, offset: 9, replay_until: Some(Cursor { inode: 7, offset: 20 }) });
        s.api.insert("def".into(), ApiSaved { since_ns: 5, replay_until_ns: None });
        s.save(&p).unwrap();
        let back = State::load(&p).unwrap();
        assert_eq!(back.cursors["abc"].replay_until, Some(Cursor { inode: 7, offset: 20 }));
        assert_eq!(back.api["def"].since_ns, 5);
        std::fs::write(&p, r#"{"version":1,"cursors":{"x":{"inode":1,"offset":2}}}"#).unwrap();
        let old = State::load(&p).unwrap();
        assert_eq!(old.cursors["x"], Saved { inode: 1, offset: 2, replay_until: None });
    }
}
