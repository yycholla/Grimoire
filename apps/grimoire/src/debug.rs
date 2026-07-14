use std::collections::{BTreeMap, VecDeque};

use grimoire_core::{Event, MemberId, MetricsSnapshot};

pub const HISTORY_LEN: usize = 120; // ~2 min at 1 s cadence
pub const EVENT_LOG_LEN: usize = 500;
// consumed by debug rendering (tasks 7-9)
#[allow(dead_code)]
pub const SPARKLINE_LEN: usize = 60; // samples shown in expanded peer rows

// Fields are populated now but only read by debug rendering (tasks 7-9).
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct DebugEvent {
    pub at: std::time::Instant,
    pub summary: String,
    pub detail: String,
}

#[derive(Default)]
pub struct DebugState {
    pub current: Option<MetricsSnapshot>,
    pub previous: Option<MetricsSnapshot>,
    pub db_bytes_history: VecDeque<u64>,
    pub send_rate_history: VecDeque<u64>,
    pub recv_rate_history: VecDeque<u64>,
    pub rtt_history: BTreeMap<MemberId, VecDeque<u64>>,
    pub events: VecDeque<DebugEvent>,
}

fn push_capped<T>(buffer: &mut VecDeque<T>, value: T, cap: usize) {
    if buffer.len() == cap {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

impl DebugState {
    pub fn apply_metrics(&mut self, snapshot: MetricsSnapshot) {
        let previous = self.current.replace(snapshot);
        let (send_delta, recv_delta) = match previous {
            Some(prev) => (
                snapshot.messages_sent.saturating_sub(prev.messages_sent),
                snapshot
                    .messages_received
                    .saturating_sub(prev.messages_received),
            ),
            None => (0, 0),
        };
        self.previous = previous;
        push_capped(&mut self.db_bytes_history, snapshot.db_bytes, HISTORY_LEN);
        push_capped(&mut self.send_rate_history, send_delta, HISTORY_LEN);
        push_capped(&mut self.recv_rate_history, recv_delta, HISTORY_LEN);
    }

    pub fn push_rtt(&mut self, member: MemberId, rtt_ms: u64) {
        let buffer = self.rtt_history.entry(member).or_default();
        push_capped(buffer, rtt_ms, HISTORY_LEN);
    }

    pub fn push_event(&mut self, summary: String, detail: String) {
        push_capped(
            &mut self.events,
            DebugEvent {
                at: std::time::Instant::now(),
                summary,
                detail,
            },
            EVENT_LOG_LEN,
        );
    }
}

/// A short human-readable line summarizing an emitted [`Event`].
pub fn event_summary(event: &Event) -> String {
    match event {
        Event::PeerConnected(member) => format!("peer connected {}", short_member(member)),
        Event::DisplayNameChanged { name, .. } => {
            format!("display name changed to {}", name.as_str())
        }
        Event::ChannelCreated(channel) => format!("channel created {}", channel.name()),
        Event::TextStored(_) => "message stored".to_string(),
        Event::AttachmentStored(_) => "attachment stored".to_string(),
        Event::AttachmentForgotten { .. } => "attachment forgotten".to_string(),
        Event::VoiceReceived(_) => "voice frame received".to_string(),
        Event::VoicePresence { state, .. } => format!("voice presence changed: {state:?}"),
        Event::MembershipChanged(_) => "membership changed".to_string(),
        Event::Fault(_) => "fault".to_string(),
    }
}

fn short_member(member: &MemberId) -> String {
    let bytes = member.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use grimoire_core::{MemberId, MetricsSnapshot};

    #[test]
    fn ring_buffers_cap_at_history_len() {
        let mut debug = DebugState::default();
        for i in 0..(HISTORY_LEN as u64 + 10) {
            let snapshot = MetricsSnapshot {
                db_bytes: i,
                ..Default::default()
            };
            debug.apply_metrics(snapshot);
        }
        assert_eq!(debug.db_bytes_history.len(), HISTORY_LEN);
        assert_eq!(
            *debug.db_bytes_history.back().unwrap(),
            HISTORY_LEN as u64 + 9
        );
    }

    #[test]
    fn rates_are_deltas_between_snapshots() {
        let mut debug = DebugState::default();
        debug.apply_metrics(MetricsSnapshot {
            messages_sent: 10,
            messages_received: 4,
            ..Default::default()
        });
        assert_eq!(debug.send_rate_history.back(), Some(&0));
        debug.apply_metrics(MetricsSnapshot {
            messages_sent: 13,
            messages_received: 9,
            ..Default::default()
        });
        assert_eq!(debug.send_rate_history.back(), Some(&3));
        assert_eq!(debug.recv_rate_history.back(), Some(&5));
    }

    #[test]
    fn event_log_caps_and_keeps_newest() {
        let mut debug = DebugState::default();
        for i in 0..(EVENT_LOG_LEN + 5) {
            debug.push_event(format!("event {i}"), String::new());
        }
        assert_eq!(debug.events.len(), EVENT_LOG_LEN);
        assert!(
            debug
                .events
                .back()
                .unwrap()
                .summary
                .ends_with(&format!("{}", EVENT_LOG_LEN + 4))
        );
    }

    #[test]
    fn rtt_history_tracked_per_member() {
        let mut debug = DebugState::default();
        let member = MemberId::from_bytes([7u8; 32]);
        debug.push_rtt(member, 12);
        debug.push_rtt(member, 15);
        assert_eq!(debug.rtt_history.get(&member).unwrap().len(), 2);
    }

    #[test]
    fn short_member_formats_leading_bytes_as_hex() {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&[0xab, 0xcd, 0x01, 0xef]);
        let member = MemberId::from_bytes(bytes);
        assert_eq!(super::short_member(&member), "abcd01ef");
    }

    #[test]
    fn event_summary_peer_connected_includes_member_hex() {
        let member = MemberId::from_bytes([7u8; 32]);
        let summary = event_summary(&Event::PeerConnected(member));
        assert!(summary.starts_with("peer connected"));
        assert!(summary.ends_with("07070707"), "got {summary:?}");
    }

    #[test]
    fn event_summary_display_name_changed_includes_name() {
        let member = MemberId::from_bytes([1u8; 32]);
        let name = grimoire_core::DisplayName::new("Alice").unwrap();
        assert_eq!(
            event_summary(&Event::DisplayNameChanged { member, name }),
            "display name changed to Alice"
        );
    }

    #[test]
    fn event_summary_channel_created_includes_name() {
        let channel = grimoire_core::Channel::new(
            grimoire_core::ChannelId::GENERAL,
            "random-chat",
            grimoire_core::ChannelKind::Text,
        )
        .unwrap();
        assert_eq!(
            event_summary(&Event::ChannelCreated(channel)),
            "channel created random-chat"
        );
    }

    #[test]
    fn event_summary_voice_presence_includes_state() {
        let summary = event_summary(&Event::VoicePresence {
            channel: grimoire_core::ChannelId::VOICE_ROOM,
            member: MemberId::from_bytes([2u8; 32]),
            state: grimoire_core::VoicePresence::Joined,
        });
        assert!(summary.starts_with("voice presence changed"));
        assert!(summary.contains("Joined"), "got {summary:?}");
    }
}
