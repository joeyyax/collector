use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// A byte position in one log file, identified by inode so it survives rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub inode: u64,
    pub offset: u64,
}

/// Where each container's oldest undelivered record starts.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub cursors: BTreeMap<String, Cursor>,
}

impl State {
    /// `Ok(None)` when there is no file yet.
    pub fn load(path: &Path) -> Result<Option<State>, String> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(self)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent().and_then(|d| File::open(d).ok()) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// Docker names rotations `<id>-json.log.1` and up.
const MAX_ROTATIONS: u32 = 32;

struct Open {
    file: File,
    inode: u64,
}

/// Opens the current file, then older rotations, so a rotation mid-scan shows a file twice rather than not at all. Oldest first.
fn scan(current: &Path) -> Vec<Open> {
    let mut files: Vec<Open> = Vec::new();
    for n in 0..=MAX_ROTATIONS {
        let p = if n == 0 { current.to_path_buf() } else { PathBuf::from(format!("{}.{n}", current.display())) };
        let Ok(file) = File::open(&p) else { continue };
        let Ok(md) = file.metadata() else { continue };
        if !files.iter().any(|f| f.inode == md.ino()) {
            files.push(Open { file, inode: md.ino() });
        }
    }
    files.reverse();
    files
}

fn size(f: &Open) -> u64 {
    f.file.metadata().map(|m| m.len()).unwrap_or(0)
}

pub enum Start {
    Beginning,
    End,
}

/// A container's log files from the cursor's onward, read through open handles.
pub struct Tail {
    path: PathBuf,
    /// Oldest first; `files[0]` holds the cursor.
    files: Vec<Open>,
    pub cursor: Cursor,
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
    /// Files that rotated away before they were read.
    pub lost: u64,
    /// Reading reached the end of the newest file; a trailing partial line stays unread.
    pub eof: bool,
}

impl Chunk {
    pub fn line(&self, l: &Line) -> &[u8] {
        &self.buf[l.range.clone()]
    }
}

impl Tail {
    /// Resumes at `saved`, or at `start` without one; `None` while there is no log file. The flag is set when `saved`'s file is gone.
    pub fn open(current: &Path, saved: Option<Cursor>, start: Start) -> Option<(Tail, bool)> {
        let mut files = scan(current);
        let first = files.first()?.inode;
        let found = saved.and_then(|c| files.iter().position(|f| f.inode == c.inode).map(|i| (i, c)));
        let (skip, cursor) = match (found, start) {
            (Some(f), _) => f,
            (None, _) if saved.is_some() => (0, Cursor { inode: first, offset: 0 }),
            (None, Start::Beginning) => (0, Cursor { inode: first, offset: 0 }),
            (None, Start::End) => {
                let last = files.last()?;
                (files.len() - 1, Cursor { inode: last.inode, offset: size(last) })
            }
        };
        files.drain(..skip);
        Some((Tail { path: current.to_path_buf(), files, cursor }, saved.is_some() && found.is_none()))
    }

    /// Picks up rotations. Keeps the cursor's file even once deleted; returns how many newer files were deleted unread.
    fn sync(&mut self) -> u64 {
        let newest = self.files.last().map(|f| f.inode);
        if std::fs::metadata(&self.path).ok().map(|m| m.ino()) == newest {
            return 0;
        }
        let scanned = scan(&self.path);
        let mut next = Vec::with_capacity(scanned.len() + 1);
        match scanned.iter().position(|f| f.inode == self.cursor.inode) {
            Some(i) => next.extend(scanned.into_iter().skip(i)),
            None => {
                next.push(self.files.remove(0));
                next.extend(scanned);
            }
        }
        let lost = self.files.iter().filter(|f| !next.iter().any(|n| n.inode == f.inode)).count();
        self.files = next;
        lost as u64
    }

    /// Reads up to `budget` bytes of complete lines after the cursor, following rotations.
    pub fn read(&mut self, budget: usize) -> Chunk {
        let lost = self.sync();
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        loop {
            let f = &self.files[0];
            let len = size(f);
            if len < self.cursor.offset {
                self.cursor.offset = 0;
            }
            let rotated = self.files.len() > 1;
            let left = len - self.cursor.offset;
            let want = left.min(budget.saturating_sub(buf.len()) as u64) as usize;
            let limited = (want as u64) < left;
            if want > 0 {
                let start = buf.len();
                buf.resize(start + want, 0);
                let n = read_full_at(&f.file, &mut buf[start..], self.cursor.offset);
                buf.truncate(start + n);
                let at_eof = self.cursor.offset + n as u64 >= len;
                let mut pos = start;
                while let Some(i) = buf[pos..].iter().position(|&b| b == b'\n') {
                    let s = Cursor { inode: f.inode, offset: self.cursor.offset + (pos - start) as u64 };
                    let e = Cursor { inode: f.inode, offset: s.offset + i as u64 + 1 };
                    lines.push(Line { range: pos..pos + i, start: s, end: e });
                    pos += i + 1;
                }
                let complete = pos - start;
                buf.truncate(pos);
                if complete == 0 && start == 0 && n >= budget {
                    // A single line longer than the budget: skip it rather than stall.
                    self.cursor.offset += n as u64;
                } else if rotated && at_eof {
                    // Rotated files are final; drop any trailing partial line.
                    self.cursor.offset = len;
                } else {
                    self.cursor.offset += complete as u64;
                }
            }
            let drained = self.cursor.offset >= len;
            if drained && rotated && buf.len() < budget {
                self.files.remove(0);
                self.cursor = Cursor { inode: self.files[0].inode, offset: 0 };
                continue;
            }
            return Chunk { buf, lines, lost, eof: !rotated && !limited };
        }
    }
}

fn read_full_at(f: &File, buf: &mut [u8], offset: u64) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], offset + n as u64) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tail(log: &Path, start: Start) -> Tail {
        Tail::open(log, None, start).unwrap().0
    }

    fn ino(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().ino()
    }

    fn rotate(log: &Path) {
        std::fs::rename(log, format!("{}.1", log.display())).unwrap();
    }

    #[test]
    fn reads_complete_lines_and_resumes() {
        let d = dir("resume");
        let log = d.join("c-json.log");
        append(&log, "one\ntwo\nthr");
        let mut t = tail(&log, Start::Beginning);
        let r = t.read(1024);
        assert_eq!(text(&r), "one\ntwo\n");
        assert_eq!(t.cursor.offset, 8);
        assert_eq!((r.lines[1].start.offset, r.lines[1].end.offset), (4, 8));
        assert!(r.eof);
        append(&log, "ee\n");
        assert_eq!(text(&t.read(1024)), "three\n");
        assert!(t.read(1024).lines.is_empty());
    }

    #[test]
    fn respects_budget() {
        let d = dir("budget");
        let log = d.join("c-json.log");
        append(&log, "aaaa\nbbbb\ncccc\n");
        let mut t = tail(&log, Start::Beginning);
        let r = t.read(11);
        assert_eq!(text(&r), "aaaa\nbbbb\n");
        assert!(!r.eof);
        assert_eq!(text(&t.read(11)), "cccc\n");
    }

    #[test]
    fn follows_rotation_without_loss() {
        let d = dir("rotate");
        let log = d.join("c-json.log");
        append(&log, "a\nb\n");
        let mut t = tail(&log, Start::Beginning);
        assert_eq!(text(&t.read(2)), "a\n");
        append(&log, "c\n");
        rotate(&log);
        append(&log, "d\n");
        let r = t.read(1024);
        assert_eq!(r.lost, 0);
        assert_eq!(text(&r), "b\nc\nd\n");
        assert_ne!(r.lines[1].start.inode, r.lines[2].start.inode);
        assert_eq!(r.lines[2].start, Cursor { inode: ino(&log), offset: 0 });
    }

    #[test]
    fn rotation_between_open_and_read_keeps_identity() {
        let d = dir("race");
        let log = d.join("c-json.log");
        append(&log, "old-1\n");
        let old = ino(&log);
        let mut t = tail(&log, Start::Beginning);
        append(&log, "old-2\n");
        rotate(&log);
        append(&log, "new-1\n");
        let r = t.read(1024);
        assert_eq!(text(&r), "old-1\nold-2\nnew-1\n");
        assert_eq!(r.lines[1].start, Cursor { inode: old, offset: 6 });
        assert_eq!(r.lines[2].start, Cursor { inode: ino(&log), offset: 0 });
    }

    #[test]
    fn drains_a_deleted_file_through_its_handle() {
        let d = dir("deleted");
        let log = d.join("c-json.log");
        append(&log, "a\n");
        let mut t = tail(&log, Start::Beginning);
        assert_eq!(text(&t.read(1024)), "a\n");
        append(&log, "last\n");
        std::fs::remove_dir_all(&d).unwrap();
        let r = t.read(1024);
        assert_eq!(text(&r), "last\n");
        assert!(r.eof);
    }

    #[test]
    fn keeps_the_cursor_file_past_max_file() {
        let d = dir("maxfile");
        let log = d.join("c-json.log");
        append(&log, "a\nb\n");
        let mut t = tail(&log, Start::Beginning);
        assert_eq!(text(&t.read(2)), "a\n");
        std::fs::remove_file(&log).unwrap();
        append(&log, "c\n");
        let r = t.read(1024);
        assert_eq!((text(&r).as_str(), r.lost), ("b\nc\n", 0));
    }

    #[test]
    fn counts_newer_files_deleted_unread() {
        let d = dir("unread");
        let log = d.join("c-json.log");
        append(&log, "a\nb\n");
        let mut t = tail(&log, Start::Beginning);
        assert_eq!(text(&t.read(2)), "a\n");
        rotate(&log);
        append(&log, "c\n");
        t.sync();
        std::fs::remove_file(format!("{}.1", log.display())).unwrap();
        rotate(&log);
        std::fs::remove_file(format!("{}.1", log.display())).unwrap();
        append(&log, "d\n");
        let r = t.read(1024);
        assert_eq!((text(&r).as_str(), r.lost), ("b\nd\n", 1));
    }

    #[test]
    fn end_start_skips_history() {
        let d = dir("end");
        let log = d.join("c-json.log");
        append(&log, "old\n");
        let mut t = tail(&log, Start::End);
        append(&log, "new\n");
        assert_eq!(text(&t.read(1024)), "new\n");
    }

    #[test]
    fn reports_lost_cursor() {
        let d = dir("lost");
        let log = d.join("c-json.log");
        append(&log, "x\n");
        let (mut t, lost) = Tail::open(&log, Some(Cursor { inode: 1, offset: 5 }), Start::End).unwrap();
        assert!(lost);
        assert_eq!(text(&t.read(1024)), "x\n");
        let saved = Some(Cursor { inode: ino(&log), offset: 2 });
        assert!(!Tail::open(&log, saved, Start::Beginning).unwrap().1);
    }

    #[test]
    fn state_round_trips_and_reads_old_formats() {
        let d = dir("state");
        let p = d.join("cursors.json");
        let mut s = State { version: 2, ..Default::default() };
        s.cursors.insert("abc".into(), Cursor { inode: 7, offset: 9 });
        s.save(&p).unwrap();
        assert!(!p.with_extension("tmp").exists());
        assert_eq!(State::load(&p).unwrap().unwrap().cursors["abc"], Cursor { inode: 7, offset: 9 });
        std::fs::write(&p, r#"{"version":2,"cursors":{"x":{"inode":1,"offset":2,"replay_until":{"inode":1,"offset":9}}},"api":{"y":{"since_ns":5}}}"#).unwrap();
        assert_eq!(State::load(&p).unwrap().unwrap().cursors["x"], Cursor { inode: 1, offset: 2 });
    }

    #[test]
    fn missing_and_corrupt_state_differ() {
        let d = dir("corrupt");
        let p = d.join("cursors.json");
        assert!(State::load(&p).unwrap().is_none());
        std::fs::write(&p, "").unwrap();
        assert!(State::load(&p).is_err());
    }
}
