use std::collections::{BTreeMap, VecDeque};

use gpui::prelude::*;
use gpui::{AnyElement, Context, SharedString, div, px, rgb};
use grimoire_core::{Event, MemberId, MetricsSnapshot};

use crate::Shell;

pub const HISTORY_LEN: usize = 120; // ~2 min at 1 s cadence
pub const EVENT_LOG_LEN: usize = 500;
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DebugPage {
    #[default]
    Overview,
    Connections,
    Storage,
    Crypto,
    Audio,
    Events,
}

impl DebugPage {
    pub const ALL: [DebugPage; 6] = [
        DebugPage::Overview,
        DebugPage::Connections,
        DebugPage::Storage,
        DebugPage::Crypto,
        DebugPage::Audio,
        DebugPage::Events,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            DebugPage::Overview => "overview",
            DebugPage::Connections => "connections",
            DebugPage::Storage => "storage",
            DebugPage::Crypto => "crypto",
            DebugPage::Audio => "audio",
            DebugPage::Events => "events",
        }
    }
}

/// Full-screen debug hub: header, page sidebar, and the active page body.
pub fn debug_view(shell: &mut Shell, cx: &mut Context<Shell>) -> impl IntoElement {
    let page = shell.debug_page;
    let nav = sidebar(shell, cx);
    let body = page_body(shell, cx);
    div()
        .id("debug-root")
        .key_context("GrimoireShell")
        .on_action(cx.listener(|this, _: &crate::ToggleDebug, _, cx| {
            this.debug_open = !this.debug_open;
            cx.notify();
        }))
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(rgb(crate::BG))
        .font_family("monospace")
        .text_size(px(13.0))
        .text_color(rgb(crate::TEXT))
        .child(
            div()
                .flex()
                .items_center()
                .h(px(38.0))
                .px(px(16.0))
                .gap(px(12.0))
                .border_b_1()
                .border_color(rgb(crate::BORDER))
                .child(div().text_color(rgb(crate::BRIGHT)).child("debug"))
                .child(
                    div()
                        .text_color(rgb(crate::MUTED))
                        .child(SharedString::from(format!("· {}", page.label()))),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("debug-close")
                        .text_color(rgb(crate::SECONDARY))
                        .hover(|style| style.text_color(rgb(crate::BRIGHT)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.debug_open = false;
                            cx.notify();
                        }))
                        .child("✕"),
                ),
        )
        .child(div().flex().flex_1().min_h_0().child(nav).child(body))
}

fn sidebar(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
    let active = shell.debug_page;
    div()
        .flex()
        .flex_col()
        .w(px(160.0))
        .h_full()
        .py(px(10.0))
        .border_r_1()
        .border_color(rgb(crate::BORDER))
        .children(DebugPage::ALL.into_iter().map(move |page| {
            let color = if page == active {
                crate::GREEN
            } else {
                crate::SECONDARY
            };
            div()
                .id(SharedString::from(format!("debug-nav-{}", page.label())))
                .px(px(16.0))
                .py(px(6.0))
                .text_color(rgb(color))
                .hover(|style| style.text_color(rgb(crate::BRIGHT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.debug_page = page;
                    cx.notify();
                }))
                .child(page.label())
        }))
}

fn page_body(shell: &mut Shell, cx: &mut Context<Shell>) -> AnyElement {
    let body = match shell.debug_page {
        DebugPage::Overview => overview_page(shell, cx).into_any_element(),
        DebugPage::Connections => connections_page(shell, cx).into_any_element(),
        DebugPage::Storage => storage_page(shell).into_any_element(),
        DebugPage::Crypto => crypto_page(shell).into_any_element(),
        DebugPage::Audio => audio_page(shell).into_any_element(),
        DebugPage::Events => events_page(shell, cx).into_any_element(),
    };
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .p(px(6.0))
        .child(body)
        .into_any_element()
}

fn overview_page(shell: &Shell, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
    let metrics = shell.debug.current.unwrap_or_default();
    let peer_count = shell
        .state
        .as_ref()
        .map(|s| s.peer_diagnostics().len())
        .unwrap_or(0);
    let events_headline = shell
        .debug
        .events
        .back()
        .map(|event| event.summary.clone())
        .unwrap_or_else(|| "no events yet".to_string());

    let connections = overview_card(
        DebugPage::Connections,
        "connections",
        SharedString::from(format!("{peer_count} peers")),
        None,
        cx,
    );
    let storage = overview_card(
        DebugPage::Storage,
        "storage",
        SharedString::from(format!(
            "{} · {} msgs",
            format_bytes(metrics.db_bytes),
            metrics.messages_total
        )),
        Some(sparkline(&shell.debug.db_bytes_history, crate::GREEN).into_any_element()),
        cx,
    );
    let crypto = overview_card(
        DebugPage::Crypto,
        "crypto",
        SharedString::from(format!(
            "epoch {} · rev {}",
            metrics.content_epoch, metrics.membership_revision
        )),
        None,
        cx,
    );
    let audio = overview_card(
        DebugPage::Audio,
        "audio",
        SharedString::from(format!(
            "{} sent · {} recv",
            metrics.voice_frames_sent, metrics.voice_frames_received
        )),
        None,
        cx,
    );
    let events = overview_card(
        DebugPage::Events,
        "events",
        SharedString::from(events_headline),
        None,
        cx,
    );

    div()
        .flex()
        .flex_wrap()
        .gap(px(10.0))
        .p(px(8.0))
        .child(connections)
        .child(storage)
        .child(crypto)
        .child(audio)
        .child(events)
}

fn overview_card(
    page: DebugPage,
    title: &'static str,
    headline: SharedString,
    extra: Option<AnyElement>,
    cx: &mut Context<Shell>,
) -> impl IntoElement + use<> {
    div()
        .id(SharedString::from(format!("debug-card-{}", page.label())))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .w(px(220.0))
        .p(px(14.0))
        .border_1()
        .border_color(rgb(crate::BORDER))
        .rounded(px(6.0))
        .hover(|style| style.border_color(rgb(crate::BORDER_BRIGHT)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.debug_page = page;
            cx.notify();
        }))
        .child(div().text_color(rgb(crate::MUTED)).child(title))
        .child(div().text_color(rgb(crate::TEXT)).child(headline))
        .children(extra)
}

fn sparkline(history: &VecDeque<u64>, color: u32) -> impl IntoElement {
    let samples: Vec<u64> = history
        .iter()
        .rev()
        .take(SPARKLINE_LEN)
        .rev()
        .copied()
        .collect();
    let max = samples.iter().copied().max().unwrap_or(1).max(1);
    div()
        .flex()
        .items_end()
        .gap(px(1.0))
        .h(px(24.0))
        .children(samples.into_iter().map(move |value| {
            let height = ((value as f32 / max as f32) * 22.0).max(1.0);
            div().w(px(2.0)).h(px(height)).bg(rgb(color))
        }))
}

fn format_bytes(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.1} KB", bytes as f64 / 1024.0),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", bytes as f64 / 1_073_741_824.0),
    }
}

// used by task 7-8 pages
#[allow(dead_code)]
fn stat(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .gap(px(8.0))
        .py(px(2.0))
        .child(
            div()
                .w(px(180.0))
                .text_color(rgb(crate::MUTED))
                .child(label),
        )
        .child(div().text_color(rgb(crate::TEXT)).child(value))
}

fn placeholder(label: &'static str) -> impl IntoElement {
    div().p(px(14.0)).text_color(rgb(crate::MUTED)).child(label)
}

fn connections_page(_shell: &mut Shell, _cx: &mut Context<Shell>) -> impl IntoElement {
    placeholder("connections — task 7")
}

fn storage_page(_shell: &Shell) -> impl IntoElement {
    placeholder("storage — task 8")
}

fn crypto_page(_shell: &Shell) -> impl IntoElement {
    placeholder("crypto — task 8")
}

fn audio_page(_shell: &Shell) -> impl IntoElement {
    placeholder("audio — task 9")
}

fn events_page(_shell: &mut Shell, _cx: &mut Context<Shell>) -> impl IntoElement {
    placeholder("events — task 9")
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
