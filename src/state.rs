use crate::containers::{ContainerSnapshot, StoppedSnapshot};
use crate::host::gpu::Gpu;
use crate::host::HostSnapshot;
use crate::metrics::MetricSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

#[derive(Default)]
pub struct Snapshot {
    pub host: Option<HostSnapshot>,
    pub containers: Vec<ContainerSnapshot>,
    pub stopped: Vec<StoppedSnapshot>,
    pub gpus: Vec<Gpu>,
    /// Whether per-container GPU memory is being read; None without a GPU watcher.
    pub gpu_per_container: Option<bool>,
    pub metrics: Arc<MetricSet>,
    pub collected_at: f64,
    pub collect_seconds: f64,
}

#[derive(Default)]
pub struct RemoteStats {
    pub pushes: AtomicU64,
    pub errors: AtomicU64,
    pub last_success: AtomicU64,
    pub dropped_samples: AtomicU64,
    pub queue_len: AtomicU64,
    pub queue_bytes: AtomicU64,
    pub stopped: std::sync::atomic::AtomicBool,
}

/// Stream subscribers and a counter of stored snapshots they wait on.
#[derive(Default)]
pub struct Live {
    subscribers: AtomicUsize,
    seq: Mutex<u64>,
    cv: Condvar,
}

impl Live {
    pub fn subscribers(&self) -> usize {
        self.subscribers.load(Ordering::SeqCst)
    }

    /// Counts a subscriber until the returned guard drops; None when `max` are already subscribed.
    pub fn subscribe(self: &Arc<Self>, max: usize) -> Option<Subscription> {
        self.subscribers.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1)).ok()?;
        Some(Subscription(self.clone()))
    }

    pub fn seq(&self) -> u64 {
        *self.seq.lock().unwrap()
    }

    /// Waits up to `timeout` for a snapshot newer than `seen`; returns the new sequence number.
    pub fn wait(&self, seen: u64, timeout: Duration) -> Option<u64> {
        let g = self.seq.lock().unwrap();
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |s| *s == seen).unwrap();
        (*g != seen).then_some(*g)
    }

    fn bump(&self) {
        *self.seq.lock().unwrap() += 1;
        self.cv.notify_all();
    }
}

pub struct Subscription(Arc<Live>);

impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
pub struct Shared {
    pub snap: Arc<RwLock<Snapshot>>,
    pub remote: Arc<RemoteStats>,
    pub live: Arc<Live>,
}

impl Shared {
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Snapshot> {
        self.snap.read().unwrap()
    }

    pub fn store(&self, s: Snapshot) {
        *self.snap.write().unwrap() = s;
        self.live.bump();
    }

    pub fn remote_emit(&self, m: &mut MetricSet) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
        m.counter("collector_remote_write_pushes_total", "Successful remote-write pushes.", &[], g(&self.remote.pushes));
        m.counter("collector_remote_write_errors_total", "Failed remote-write pushes.", &[], g(&self.remote.errors));
        m.gauge("collector_remote_write_last_success_timestamp_seconds", "Time of the last successful remote-write push.", &[], g(&self.remote.last_success));
        m.counter("collector_remote_write_dropped_samples_total", "Samples dropped because the receiver rejected them, the retry queue was full or shutdown came first.", &[], g(&self.remote.dropped_samples));
        m.gauge("collector_remote_write_queue_pushes", "Pushes waiting to be sent.", &[], g(&self.remote.queue_len));
        m.gauge("collector_remote_write_queue_bytes", "Compressed bytes waiting to be sent.", &[], g(&self.remote.queue_bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribers_are_capped_and_released() {
        let live = Arc::new(Live::default());
        let a = live.subscribe(2).unwrap();
        let _b = live.subscribe(2).unwrap();
        assert!(live.subscribe(2).is_none());
        drop(a);
        assert_eq!(live.subscribers(), 1);
        assert!(live.subscribe(2).is_some());
    }

    #[test]
    fn waiters_wake_on_store() {
        let shared = Shared::default();
        let seen = shared.live.seq();
        assert_eq!(shared.live.wait(seen, Duration::from_millis(10)), None);
        let s2 = shared.clone();
        let t = std::thread::spawn(move || s2.live.wait(seen, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        shared.store(Snapshot { collected_at: 1.0, ..Default::default() });
        assert_eq!(t.join().unwrap(), Some(seen + 1));
    }
}
