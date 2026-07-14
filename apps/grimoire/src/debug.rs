use std::collections::{BTreeMap, VecDeque};

use gpui::prelude::*;
use gpui::{AnyElement, Context, SharedString, div, px, rgb};
use grimoire_core::{ConnectionPathKind, Event, MemberId, MetricsSnapshot};

use crate::Shell;
use crate::state::CommunityState;

pub const HISTORY_LEN: usize = 120; // ~2 min at 1 s cadence
pub const EVENT_LOG_LEN: usize = 500;
pub const SPARKLINE_LEN: usize = 60; // samples shown in expanded peer rows

#[derive(Clone, Debug)]
pub struct DebugEvent {
    pub at: std::time::Instant,
    pub summary: String,
    pub detail: String,
}

#[derive(Default)]
pub struct DebugState {
    pub current: Option<MetricsSnapshot>,
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
        let (send_delta, recv_delta) = match self.current.replace(snapshot) {
            Some(prev) => (
                snapshot.messages_sent.saturating_sub(prev.messages_sent),
                snapshot
                    .messages_received
                    .saturating_sub(prev.messages_received),
            ),
            None => (0, 0),
        };
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

fn sparkline(history: &VecDeque<u64>, color: u32) -> impl IntoElement + use<> {
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

/// A stat row whose value turns red once the count is nonzero, so failure
/// counters stand out.
fn stat_count(label: &'static str, value: u64) -> impl IntoElement {
    let color = if value > 0 { crate::RED } else { crate::TEXT };
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
        .child(div().text_color(rgb(color)).child(value.to_string()))
}

fn placeholder(label: &'static str) -> impl IntoElement {
    div().p(px(14.0)).text_color(rgb(crate::MUTED)).child(label)
}

struct PeerRow {
    member: MemberId,
    name: String,
    selected_kind: Option<ConnectionPathKind>,
    selected_rtt_ms: u64,
    paths: Vec<(ConnectionPathKind, bool, u64)>,
    rtt_history: VecDeque<u64>,
    expanded: bool,
    full_hex: String,
}

fn display_name(state: &CommunityState, member: MemberId) -> String {
    state
        .members
        .iter()
        .find(|entry| entry.id == member)
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| short_member(&member))
}

fn kind_label(kind: ConnectionPathKind) -> &'static str {
    match kind {
        ConnectionPathKind::Direct => "direct",
        ConnectionPathKind::Relay => "relay",
        ConnectionPathKind::Custom => "custom",
    }
}

fn connections_page(shell: &mut Shell, cx: &mut Context<Shell>) -> AnyElement {
    let Some(state) = shell.state.as_ref() else {
        return placeholder("no active session").into_any_element();
    };
    let expanded = shell.expanded_peer;
    let snapshot = shell.debug.current.unwrap_or_default();

    // Extract everything owned up front so element listeners (which need
    // &mut Shell via cx) don't collide with borrows of `shell`.
    let rows: Vec<PeerRow> = state
        .peer_diagnostics()
        .iter()
        .map(|peer| {
            let member = peer.member();
            let paths: Vec<(ConnectionPathKind, bool, u64)> = peer
                .paths()
                .iter()
                .map(|path| {
                    (
                        path.kind(),
                        path.is_selected(),
                        path.rtt().as_millis() as u64,
                    )
                })
                .collect();
            let selected = paths
                .iter()
                .copied()
                .find(|(_, is_selected, _)| *is_selected);
            PeerRow {
                member,
                name: display_name(state, member),
                selected_kind: selected.map(|(kind, _, _)| kind),
                selected_rtt_ms: selected.map(|(_, _, rtt)| rtt).unwrap_or(0),
                paths,
                // Only the expanded peer renders a sparkline, so only it needs
                // its history cloned.
                rtt_history: if expanded == Some(member) {
                    shell
                        .debug
                        .rtt_history
                        .get(&member)
                        .cloned()
                        .unwrap_or_default()
                } else {
                    VecDeque::new()
                },
                expanded: expanded == Some(member),
                full_hex: member
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            }
        })
        .collect();

    let max_rtt = rows
        .iter()
        .map(|row| row.selected_rtt_ms)
        .max()
        .unwrap_or(1)
        .max(1);

    let bars = div()
        .flex()
        .items_end()
        .gap(px(10.0))
        .h(px(90.0))
        .children(rows.iter().map(|row| {
            let member = row.member;
            let relay = matches!(row.selected_kind, Some(ConnectionPathKind::Relay));
            let height = ((row.selected_rtt_ms as f32 / max_rtt as f32) * 78.0).max(2.0);
            let color = if relay { crate::YELLOW } else { crate::GREEN };
            let label = format!("{} {}ms", row.name, row.selected_rtt_ms);
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "debug-conn-bar-{}",
                            short_member(&member)
                        )))
                        .w(px(26.0))
                        .h(px(height))
                        .bg(rgb(color))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.expanded_peer = if this.expanded_peer == Some(member) {
                                None
                            } else {
                                Some(member)
                            };
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(crate::SECONDARY))
                        .child(label),
                )
        }));

    let strip = div()
        .flex()
        .gap(px(18.0))
        .text_size(px(11.0))
        .text_color(rgb(crate::MUTED))
        .child(format!(
            "conns {}/{}",
            snapshot.conns_opened, snapshot.conns_closed
        ))
        .child(format!("holepunch {}", snapshot.holepunch_attempts))
        .child(format!(
            "relay ⇅ {}/{}",
            snapshot.send_relay, snapshot.recv_data_relay
        ))
        .child(format!("datagrams {}", snapshot.recv_datagrams));

    div()
        .id("debug-connections")
        .flex()
        .flex_col()
        .flex_1()
        .gap(px(12.0))
        .p(px(14.0))
        .overflow_y_scroll()
        .child(bars)
        .child(strip)
        .children(rows.iter().map(|row| {
            let member = row.member;
            let relay = matches!(row.selected_kind, Some(ConnectionPathKind::Relay));
            let kind_text = row.selected_kind.map(kind_label).unwrap_or("no path");
            let marker = if row.expanded { "▾" } else { "▸" };
            let header = format!(
                "{} {} · {} · {}ms",
                marker, row.name, kind_text, row.selected_rtt_ms
            );
            let container = div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .p(px(8.0))
                .border_1()
                .border_color(rgb(if row.expanded {
                    crate::GREEN
                } else {
                    crate::BORDER
                }))
                .rounded(px(4.0))
                // Only the header toggles; clicking the expanded detail (hex,
                // sparkline) must not collapse the row.
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "debug-conn-row-{}",
                            short_member(&member)
                        )))
                        .cursor_pointer()
                        .text_color(rgb(crate::TEXT))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.expanded_peer = if this.expanded_peer == Some(member) {
                                None
                            } else {
                                Some(member)
                            };
                            cx.notify();
                        }))
                        .child(header),
                );
            if row.expanded {
                let color = if relay { crate::YELLOW } else { crate::GREEN };
                let detail = div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .pl(px(12.0))
                    .pt(px(4.0))
                    .children(row.paths.iter().map(|(kind, is_selected, rtt)| {
                        let dot = if *is_selected { "●" } else { "○" };
                        let tail = if *is_selected { " ← selected" } else { "" };
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(crate::SECONDARY))
                            .child(format!("{} {} {}ms{}", dot, kind_label(*kind), rtt, tail))
                    }))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(crate::MUTED))
                            .child("rtt (last 60s)"),
                    )
                    .child(sparkline(&row.rtt_history, color))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(crate::MUTED))
                            .child(row.full_hex.clone()),
                    );
                container.child(detail).into_any_element()
            } else {
                container.into_any_element()
            }
        }))
        .into_any_element()
}

fn storage_page(shell: &Shell) -> impl IntoElement {
    let snapshot = shell.debug.current.unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .p(px(14.0))
        .gap(px(10.0))
        .child(div().text_color(rgb(crate::MUTED)).child("db size (2 min)"))
        .child(sparkline(&shell.debug.db_bytes_history, crate::GREEN))
        .child(stat("db size", format_bytes(snapshot.db_bytes)))
        .child(stat("messages", snapshot.messages_total.to_string()))
        .child(stat("attachments", snapshot.attachments_total.to_string()))
        .child(stat("channels", snapshot.channels_total.to_string()))
        .child(stat("members", snapshot.members_total.to_string()))
}

fn crypto_page(shell: &Shell) -> impl IntoElement {
    let snapshot = shell.debug.current.unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .p(px(14.0))
        .gap(px(10.0))
        .child(stat("content epoch", snapshot.content_epoch.to_string()))
        .child(stat(
            "membership revision",
            snapshot.membership_revision.to_string(),
        ))
        .child(stat("messages sent", snapshot.messages_sent.to_string()))
        .child(stat(
            "messages received",
            snapshot.messages_received.to_string(),
        ))
        .child(stat_count("decrypt failures", snapshot.decrypt_failures))
        .child(
            div()
                .text_color(rgb(crate::MUTED))
                .child("send/recv per second (2 min)"),
        )
        .child(sparkline(&shell.debug.send_rate_history, crate::GREEN))
        .child(sparkline(&shell.debug.recv_rate_history, crate::YELLOW))
}

fn audio_page(shell: &Shell) -> impl IntoElement {
    let snapshot = shell.debug.current.unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .p(px(14.0))
        .gap(px(10.0))
        .child(stat("voice state", shell.voice.status().to_string()))
        .child(stat(
            "input devices",
            shell.audio_devices.input.len().to_string(),
        ))
        .child(stat(
            "output devices",
            shell.audio_devices.output.len().to_string(),
        ))
        .child(stat("frames sent", snapshot.voice_frames_sent.to_string()))
        .child(stat(
            "frames received",
            snapshot.voice_frames_received.to_string(),
        ))
        .child(stat_count("frame failures", snapshot.voice_frame_failures))
}

struct EventRow {
    index: usize,
    age_secs: u64,
    summary: String,
    detail: String,
    expanded: bool,
}

fn events_page(shell: &mut Shell, cx: &mut Context<Shell>) -> AnyElement {
    if shell.debug.events.is_empty() {
        return placeholder("no events yet").into_any_element();
    }

    let expanded_event = shell.expanded_event;

    // Extract everything owned up front so element listeners (which need
    // &mut Shell via cx) don't collide with borrows of `shell`.
    let rows: Vec<EventRow> = shell
        .debug
        .events
        .iter()
        .rev()
        .enumerate()
        .map(|(index, event)| EventRow {
            index,
            age_secs: event.at.elapsed().as_secs(),
            summary: event.summary.clone(),
            detail: event.detail.clone(),
            expanded: expanded_event == Some(index),
        })
        .collect();

    div()
        .id("debug-events")
        .flex()
        .flex_col()
        .flex_1()
        .p(px(14.0))
        .gap(px(2.0))
        .overflow_y_scroll()
        .children(rows.into_iter().map(|row| {
            let index = row.index;
            let container = div()
                .id(SharedString::from(format!("debug-event-{index}")))
                .flex()
                .flex_col()
                .p(px(6.0))
                .border_1()
                .border_color(rgb(if row.expanded {
                    crate::GREEN
                } else {
                    crate::BORDER
                }))
                .rounded(px(4.0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.expanded_event = if this.expanded_event == Some(index) {
                        None
                    } else {
                        Some(index)
                    };
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .child(
                            div()
                                .w(px(50.0))
                                .text_color(rgb(crate::MUTED))
                                .child(format!("{}s", row.age_secs)),
                        )
                        .child(div().text_color(rgb(crate::TEXT)).child(row.summary)),
                );
            if row.expanded && !row.detail.is_empty() {
                container
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(crate::SECONDARY))
                            .child(row.detail),
                    )
                    .into_any_element()
            } else {
                container.into_any_element()
            }
        }))
        .into_any_element()
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
