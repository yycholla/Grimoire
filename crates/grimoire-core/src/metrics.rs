use std::sync::atomic::{AtomicU64, Ordering};

/// One monotonically increasing counter. Increments are relaxed atomics —
/// infallible and effectively free at call sites.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Live registry of node-level counters. Owned by `Node`, shared via `Arc`.
#[derive(Debug, Default)]
pub struct Metrics {
    pub messages_sent: Counter,
    pub messages_received: Counter,
    pub decrypt_failures: Counter,
    pub voice_frames_sent: Counter,
    pub voice_frames_received: Counter,
    pub voice_frame_failures: Counter,
}

impl Metrics {
    /// Copies counter values into a fresh snapshot. Gauge-like fields
    /// (storage totals, epoch, transport) are filled in by
    /// `Node::metrics_snapshot`.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            messages_sent: self.messages_sent.get(),
            messages_received: self.messages_received.get(),
            decrypt_failures: self.decrypt_failures.get(),
            voice_frames_sent: self.voice_frames_sent.get(),
            voice_frames_received: self.voice_frames_received.get(),
            voice_frame_failures: self.voice_frame_failures.get(),
            ..MetricsSnapshot::default()
        }
    }
}

/// Point-in-time view of all debug metrics. Plain data, cheap to clone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricsSnapshot {
    // registry counters
    pub messages_sent: u64,
    pub messages_received: u64,
    pub decrypt_failures: u64,
    pub voice_frames_sent: u64,
    pub voice_frames_received: u64,
    pub voice_frame_failures: u64,
    // storage
    pub db_bytes: u64,
    pub messages_total: u64,
    pub attachments_total: u64,
    pub channels_total: u64,
    pub members_total: u64,
    // crypto
    pub content_epoch: u64,
    pub membership_revision: u64,
    // transport (selected iroh EndpointMetrics socket counters)
    pub recv_datagrams: u64,
    pub send_relay: u64,
    pub recv_data_relay: u64,
    pub holepunch_attempts: u64,
    pub conns_opened: u64,
    pub conns_closed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_flow_into_snapshot() {
        let metrics = Metrics::default();
        metrics.messages_sent.inc();
        metrics.messages_sent.inc();
        metrics.decrypt_failures.inc();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.messages_sent, 2);
        assert_eq!(snapshot.messages_received, 0);
        assert_eq!(snapshot.decrypt_failures, 1);
    }
}
