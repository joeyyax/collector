use crate::containers::ContainerSnapshot;
use crate::host::HostSnapshot;
use crate::metrics::MetricSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Default)]
pub struct Snapshot {
    pub host: Option<HostSnapshot>,
    pub containers: Vec<ContainerSnapshot>,
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

#[derive(Clone, Default)]
pub struct Shared {
    pub snap: Arc<RwLock<Snapshot>>,
    pub remote: Arc<RemoteStats>,
}

impl Shared {
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Snapshot> {
        self.snap.read().unwrap()
    }

    pub fn store(&self, s: Snapshot) {
        *self.snap.write().unwrap() = s;
    }

    pub fn remote_emit(&self, m: &mut MetricSet) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
        m.counter("collector_remote_write_pushes_total", "Successful remote-write pushes.", &[], g(&self.remote.pushes));
        m.counter("collector_remote_write_errors_total", "Failed remote-write pushes.", &[], g(&self.remote.errors));
        m.gauge("collector_remote_write_last_success_timestamp_seconds", "Time of the last successful remote-write push.", &[], g(&self.remote.last_success));
        m.counter("collector_remote_write_dropped_samples_total", "Samples dropped because the retry queue was full or shutdown came first.", &[], g(&self.remote.dropped_samples));
        m.gauge("collector_remote_write_queue_pushes", "Pushes waiting to be sent.", &[], g(&self.remote.queue_len));
        m.gauge("collector_remote_write_queue_bytes", "Compressed bytes waiting to be sent.", &[], g(&self.remote.queue_bytes));
    }
}
