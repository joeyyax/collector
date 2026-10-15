use super::docker::InspectState;
use super::{parse_rfc3339, Container};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Seconds a container keeps its lifecycle series after it stops.
pub const RETAIN_SECS: f64 = 600.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Exit {
    pub code: i64,
    /// Docker reported an OOM kill in the container after it last started.
    pub oom_killed: bool,
    /// Unix seconds.
    pub exited_at: f64,
}

/// A container that left the running set: exited, dead, paused, restarting or removed.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub meta: Arc<Container>,
    pub state: String,
    pub since: f64,
}

/// Exits and recently stopped containers, built from Docker events and inspect.
#[derive(Default)]
pub struct Tracker {
    oom: HashSet<String>,
    exits: HashMap<String, Exit>,
    stopped: HashMap<String, Stopped>,
}

impl Tracker {
    pub fn event(&mut self, id: &str, action: &str, attrs: &BTreeMap<String, String>, at: f64) {
        match action {
            "start" => {
                self.oom.remove(id);
            }
            "oom" => {
                self.oom.insert(id.to_string());
            }
            "die" => {
                let code = attrs.get("exitCode").and_then(|c| c.parse().ok()).unwrap_or(-1);
                let oom_killed = self.oom.remove(id);
                self.exits.insert(id.to_string(), Exit { code, oom_killed, exited_at: at });
            }
            _ => {}
        }
    }

    /// Takes the exit from inspect when no die event was seen, e.g. for containers stopped before the collector started.
    pub fn inspected(&mut self, id: &str, st: &InspectState) {
        if self.exits.contains_key(id) || !matches!(st.status.as_str(), "exited" | "dead" | "restarting") {
            return;
        }
        if let Some(at) = parse_rfc3339(&st.finished_at) {
            self.exits.insert(id.to_string(), Exit { code: st.exit_code, oom_killed: st.oom_killed, exited_at: at });
        }
    }

    pub fn running(&mut self, id: &str) {
        self.stopped.remove(id);
    }

    /// Records a container that is no longer running; `st` is its inspect state when Docker still has it.
    pub fn stopped(&mut self, meta: Arc<Container>, st: Option<&InspectState>, now: f64) {
        if let Some(st) = st {
            self.inspected(&meta.id, st);
        }
        let state = match st {
            Some(st) if !st.status.is_empty() => st.status.clone(),
            Some(_) => "exited".to_string(),
            None => "removed".to_string(),
        };
        let since = st.and_then(|s| parse_rfc3339(&s.finished_at)).filter(|t| *t <= now && state != "paused").unwrap_or(now);
        if now - since < RETAIN_SECS {
            self.stopped.insert(meta.id.clone(), Stopped { meta, state, since });
        }
    }

    /// Drops stopped containers past retention and exits of containers no longer tracked.
    pub fn expire(&mut self, now: f64, running: &HashSet<&str>) {
        self.stopped.retain(|_, s| now - s.since < RETAIN_SECS);
        let keep = |id: &String| running.contains(id.as_str()) || self.stopped.contains_key(id);
        let exits = std::mem::take(&mut self.exits);
        self.exits = exits.into_iter().filter(|(id, _)| keep(id)).collect();
        let oom = std::mem::take(&mut self.oom);
        self.oom = oom.into_iter().filter(|id| keep(id)).collect();
    }

    pub fn exit(&self, id: &str) -> Option<Exit> {
        self.exits.get(id).copied()
    }

    /// Stopped containers, by name.
    pub fn stopped_list(&self) -> Vec<Stopped> {
        let mut v: Vec<Stopped> = self.stopped.values().cloned().collect();
        v.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(code: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("exitCode".to_string(), code.to_string())])
    }

    fn meta(id: &str) -> Arc<Container> {
        Arc::new(Container { id: id.into(), name: format!("{id}-name"), ..Default::default() })
    }

    #[test]
    fn oom_before_die_marks_the_exit() {
        let mut t = Tracker::default();
        let none = BTreeMap::new();
        t.event("a", "start", &none, 1.0);
        t.event("a", "oom", &none, 2.0);
        t.event("a", "kill", &none, 2.1);
        t.event("a", "die", &attrs("137"), 2.2);
        assert_eq!(t.exit("a"), Some(Exit { code: 137, oom_killed: true, exited_at: 2.2 }));
        // Restart policy: the next run exits cleanly and the OOM doesn't carry over.
        t.event("a", "start", &none, 3.0);
        assert_eq!(t.exit("a").unwrap().code, 137);
        t.event("a", "die", &attrs("0"), 4.0);
        assert_eq!(t.exit("a"), Some(Exit { code: 0, oom_killed: false, exited_at: 4.0 }));
    }

    #[test]
    fn oom_without_exit_is_cleared_by_a_start() {
        let mut t = Tracker::default();
        let none = BTreeMap::new();
        t.event("a", "oom", &none, 1.0);
        t.event("a", "start", &none, 2.0);
        t.event("a", "die", &attrs("1"), 3.0);
        assert!(!t.exit("a").unwrap().oom_killed);
    }

    #[test]
    fn inspect_fills_exits_events_missed() {
        let mut t = Tracker::default();
        let st = |status: &str, code| InspectState { status: status.into(), exit_code: code, oom_killed: true, finished_at: "2026-10-10T12:00:00Z".into(), ..Default::default() };
        t.inspected("a", &st("running", 0));
        assert_eq!(t.exit("a"), None);
        t.inspected("a", &st("restarting", 137));
        assert_eq!(t.exit("a"), Some(Exit { code: 137, oom_killed: true, exited_at: 1791633600.0 }));
        // An exit from a die event wins over inspect.
        t.event("b", "die", &attrs("2"), 5.0);
        t.inspected("b", &st("exited", 9));
        assert_eq!(t.exit("b").unwrap().code, 2);
    }

    #[test]
    fn stopped_containers_expire_after_retention() {
        let mut t = Tracker::default();
        let now = 1791633600.0;
        let exited = InspectState { status: "exited".into(), finished_at: "2026-10-10T12:00:00Z".into(), exit_code: 1, ..Default::default() };
        t.stopped(meta("a"), Some(&exited), now + 5.0);
        t.stopped(meta("b"), None, now);
        // Finished long before the collector saw it: not kept.
        t.stopped(meta("c"), Some(&exited), now + RETAIN_SECS + 1.0);
        let list = t.stopped_list();
        assert_eq!(list.iter().map(|s| (s.meta.id.as_str(), s.state.as_str(), s.since)).collect::<Vec<_>>(), [("a", "exited", now), ("b", "removed", now)]);
        assert_eq!(t.exit("a").unwrap().code, 1);

        let running = HashSet::new();
        t.expire(now + RETAIN_SECS - 1.0, &running);
        assert_eq!(t.stopped_list().len(), 2);
        t.expire(now + RETAIN_SECS, &running);
        assert!(t.stopped_list().is_empty());
        assert_eq!(t.exit("a"), None);
    }

    #[test]
    fn running_again_leaves_the_stopped_set_and_keeps_its_exit() {
        let mut t = Tracker::default();
        t.event("a", "die", &attrs("3"), 10.0);
        t.stopped(meta("a"), None, 10.0);
        t.running("a");
        assert!(t.stopped_list().is_empty());
        t.expire(20.0, &HashSet::from(["a"]));
        assert_eq!(t.exit("a").unwrap().code, 3);
        t.expire(20.0, &HashSet::new());
        assert_eq!(t.exit("a"), None);
    }
}
