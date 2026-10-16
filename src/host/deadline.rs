use std::collections::{HashMap, HashSet};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Done(T),
    /// No answer within the deadline, or a call from an earlier round is still stuck.
    Hung,
}

/// Runs each job on its own thread and waits at most `timeout` for all of them.
/// Keys whose earlier call never returned are skipped until it does, so a hung mount costs one parked thread, not one per round.
pub struct Deadline {
    stuck: Arc<Mutex<HashSet<String>>>,
}

impl Default for Deadline {
    fn default() -> Self {
        Self { stuck: Arc::new(Mutex::new(HashSet::new())) }
    }
}

impl Deadline {
    pub fn run<T: Send + 'static>(&self, jobs: Vec<(String, Box<dyn FnOnce() -> T + Send>)>, timeout: Duration) -> HashMap<String, Outcome<T>> {
        let (tx, rx) = channel();
        let mut out = HashMap::new();
        let mut waiting = 0;
        for (key, job) in jobs {
            if !self.stuck.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone()) {
                out.insert(key, Outcome::Hung);
                continue;
            }
            let (tx, stuck, k) = (tx.clone(), self.stuck.clone(), key.clone());
            let spawned = std::thread::Builder::new().name("statfs".into()).stack_size(64 << 10).spawn(move || {
                let v = job();
                stuck.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&k);
                let _ = tx.send((k, v));
            });
            match spawned {
                Ok(_) => waiting += 1,
                Err(_) => {
                    self.stuck.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&key);
                    out.insert(key, Outcome::Hung);
                }
            }
        }
        let end = Instant::now() + timeout;
        while waiting > 0 {
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((k, v)) => {
                    out.insert(k, Outcome::Done(v));
                    waiting -= 1;
                }
                Err(_) => break,
            }
        }
        out
    }

    pub fn stuck(&self) -> usize {
        self.stuck.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Box<dyn FnOnce() -> T + Send> {
        Box::new(f)
    }

    #[test]
    fn slow_job_is_skipped_until_it_returns() {
        let d = Deadline::default();
        let gate = Arc::new(Mutex::new(()));
        let hold = gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let g = gate.clone();
        let r = d.run(vec![("fast".into(), job(|| 1)), ("slow".into(), job(move || { drop(g.lock()); 2 }))], Duration::from_millis(100));
        assert_eq!(r["fast"], Outcome::Done(1));
        assert!(!r.contains_key("slow"));
        assert_eq!(d.stuck(), 1);
        let started = Instant::now();
        let r = d.run(vec![("slow".into(), job(|| 3))], Duration::from_secs(5));
        assert_eq!(r["slow"], Outcome::Hung);
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(hold);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(d.stuck(), 0);
        let r = d.run(vec![("slow".into(), job(|| 4))], Duration::from_secs(1));
        assert_eq!(r["slow"], Outcome::Done(4));
    }
}
