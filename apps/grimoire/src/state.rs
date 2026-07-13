use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use grimoire_audio::{VoiceDeviceConfig, VoiceSession};
use grimoire_core::{
    ChannelId, ChannelKind, Command, CommunityInvite, ConnectionPathKind, Event,
    MAX_VOICE_PARTICIPANTS, MemberId, MemberRole, MessageId, Node, NodeConfig, PeerAddress,
    PeerDiagnostic, Snapshot, VoicePresence, restore_identity,
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast, mpsc as tokio_mpsc, oneshot},
    task::JoinHandle,
};

pub struct Session {
    node: Option<Arc<Node>>,
    runtime: Option<Runtime>,
    commands: tokio_mpsc::UnboundedSender<SessionCommand>,
    updates: SessionUpdates,
    command_task: Option<JoinHandle<()>>,
    event_task: Option<JoinHandle<()>>,
}

enum SessionCommand {
    Execute(Command),
    MarkRead {
        channel: ChannelId,
        message: MessageId,
    },
    Connect(PeerAddress),
    RequestInvite,
    ExportIdentity {
        path: PathBuf,
        passphrase: String,
    },
    JoinVoice(ChannelId),
    SetVoiceMuted(bool),
    SetVoiceDeafened(bool),
    SetVoiceDevices(VoiceDeviceConfig),
    LeaveVoice,
    VoiceFinished {
        generation: u64,
        channel: ChannelId,
        result: Result<(), String>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug)]
pub enum SessionUpdate {
    Event(Event),
    CommandFailed(String),
    InviteReady(String),
    Snapshot(Snapshot),
    ReadStored {
        channel: ChannelId,
        message: MessageId,
    },
    Diagnostics(Vec<PeerDiagnostic>),
    Voice(VoiceUpdate),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceUpdate {
    Joining(ChannelId),
    Joined(ChannelId),
    Muted(bool),
    Deafened(bool),
    Leaving(ChannelId),
    Disconnected,
    Failed { channel: ChannelId, error: String },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum VoicePhase {
    #[default]
    Disconnected,
    Joining,
    Connected,
    Leaving,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoiceState {
    channel: Option<ChannelId>,
    phase: VoicePhase,
    muted: bool,
    deafened: bool,
    error: Option<String>,
}

impl VoiceState {
    pub fn apply(&mut self, update: VoiceUpdate) {
        match update {
            VoiceUpdate::Joining(channel) => {
                self.channel = Some(channel);
                self.phase = VoicePhase::Joining;
                self.muted = false;
                self.deafened = false;
                self.error = None;
            }
            VoiceUpdate::Joined(channel) => {
                self.channel = Some(channel);
                self.phase = VoicePhase::Connected;
                self.error = None;
            }
            VoiceUpdate::Muted(muted) => self.muted = muted,
            VoiceUpdate::Deafened(deafened) => self.deafened = deafened,
            VoiceUpdate::Leaving(channel) => {
                self.channel = Some(channel);
                self.phase = VoicePhase::Leaving;
            }
            VoiceUpdate::Disconnected => *self = Self::default(),
            VoiceUpdate::Failed { error, .. } => {
                self.channel = None;
                self.phase = VoicePhase::Failed;
                self.muted = false;
                self.deafened = false;
                self.error = Some(error);
            }
        }
    }

    pub const fn channel(&self) -> Option<ChannelId> {
        self.channel
    }

    pub const fn is_connected(&self) -> bool {
        matches!(self.phase, VoicePhase::Connected)
    }

    pub const fn is_muted(&self) -> bool {
        self.muted
    }

    pub const fn is_deafened(&self) -> bool {
        self.deafened
    }

    pub fn status(&self) -> &str {
        match self.phase {
            VoicePhase::Disconnected => "disconnected",
            VoicePhase::Joining => "joining",
            VoicePhase::Connected if self.deafened => "deafened",
            VoicePhase::Connected if self.muted => "muted",
            VoicePhase::Connected => "connected",
            VoicePhase::Leaving => "leaving",
            VoicePhase::Failed => self.error.as_deref().unwrap_or("voice failed"),
        }
    }
}

#[derive(Clone)]
pub struct SessionUpdates(Arc<tokio::sync::Mutex<tokio_mpsc::UnboundedReceiver<SessionUpdate>>>);

impl SessionUpdates {
    pub async fn next(&self) -> Option<SessionUpdate> {
        self.0.lock().await.recv().await
    }
}

impl Session {
    fn send(&self, command: SessionCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "community session is closed".to_owned())
    }

    pub fn execute(&self, command: Command) -> Result<(), String> {
        self.send(SessionCommand::Execute(command))
    }

    pub fn mark_read(&self, channel: ChannelId, message: MessageId) -> Result<(), String> {
        self.send(SessionCommand::MarkRead { channel, message })
    }

    pub fn connect(&self, address: PeerAddress) -> Result<(), String> {
        self.send(SessionCommand::Connect(address))
    }

    pub fn request_invite(&self) -> Result<(), String> {
        self.send(SessionCommand::RequestInvite)
    }

    pub fn export_identity(&self, path: PathBuf, passphrase: String) -> Result<(), String> {
        self.send(SessionCommand::ExportIdentity { path, passphrase })
    }

    pub fn join_voice(&self, channel: ChannelId) -> Result<(), String> {
        self.send(SessionCommand::JoinVoice(channel))
    }

    pub fn set_voice_muted(&self, muted: bool) -> Result<(), String> {
        self.send(SessionCommand::SetVoiceMuted(muted))
    }

    pub fn set_voice_deafened(&self, deafened: bool) -> Result<(), String> {
        self.send(SessionCommand::SetVoiceDeafened(deafened))
    }

    pub fn set_voice_devices(&self, devices: VoiceDeviceConfig) -> Result<(), String> {
        self.send(SessionCommand::SetVoiceDevices(devices))
    }

    pub fn leave_voice(&self) -> Result<(), String> {
        self.send(SessionCommand::LeaveVoice)
    }

    #[cfg(test)]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<SessionUpdate, String> {
        self.runtime
            .as_ref()
            .ok_or_else(|| "community session is closed".to_owned())?
            .block_on(async {
                tokio::time::timeout(timeout, self.updates.next())
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "community update stream is closed".to_owned())
            })
    }

    pub fn updates(&self) -> SessionUpdates {
        self.updates.clone()
    }

    pub fn close(mut self) -> Result<(), String> {
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| "community session is already closed".to_owned())?;
        let node = self
            .node
            .take()
            .ok_or_else(|| "community session has no node".to_owned())?;
        let command_task = self
            .command_task
            .take()
            .ok_or_else(|| "community command task is unavailable".to_owned())?;
        let event_task = self
            .event_task
            .take()
            .ok_or_else(|| "community event task is unavailable".to_owned())?;
        let (stopped, stopped_rx) = oneshot::channel();
        self.commands
            .send(SessionCommand::Shutdown(stopped))
            .map_err(|_| "community command task stopped early".to_owned())?;
        runtime.block_on(async move {
            stopped_rx.await.map_err(|error| error.to_string())?;
            command_task.await.map_err(|error| error.to_string())?;
            event_task.abort();
            let _ = event_task.await;
            Arc::try_unwrap(node)
                .map_err(|_| "community node is still in use".to_owned())?
                .shutdown()
                .await
                .map_err(|error| error.to_string())
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = &self.command_task {
            task.abort();
        }
        if let Some(task) = &self.event_task {
            task.abort();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelState {
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageState {
    pub id: MessageId,
    pub channel: ChannelId,
    pub author_id: MemberId,
    pub author: String,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentState {
    pub id: MessageId,
    pub channel: ChannelId,
    pub author_id: MemberId,
    pub author: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberState {
    pub id: MemberId,
    pub name: String,
    pub is_owner: bool,
    pub is_local: bool,
    pub role: MemberRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommunityAccess {
    Waiting,
    Active,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommunityState {
    pub channels: Vec<ChannelState>,
    pub messages: Vec<MessageState>,
    pub attachments: Vec<AttachmentState>,
    pub members: Vec<MemberState>,
    pub selected_channel: ChannelId,
    owner: MemberId,
    local_member: MemberId,
    access: CommunityAccess,
    online: BTreeSet<MemberId>,
    pending_members: BTreeSet<MemberId>,
    voice: BTreeMap<ChannelId, BTreeMap<MemberId, bool>>,
    speaking: BTreeMap<(ChannelId, MemberId), Instant>,
    last_read: BTreeMap<ChannelId, MessageId>,
    local_address: String,
    connection: Option<(ConnectionPathKind, u64)>,
    diagnostics: Vec<PeerDiagnostic>,
}

impl CommunityState {
    pub fn from_snapshot(snapshot: &Snapshot, local_member: MemberId) -> Self {
        let access = initial_access(
            !snapshot.channels().is_empty(),
            snapshot.members().contains(&local_member),
        );
        let channels = snapshot
            .channels()
            .iter()
            .map(|channel| ChannelState {
                id: channel.id(),
                name: channel.name().to_owned(),
                kind: channel.kind(),
            })
            .collect::<Vec<_>>();
        let selected_channel = channels
            .iter()
            .find(|channel| channel.kind == ChannelKind::Text)
            .or_else(|| channels.first())
            .map_or(ChannelId::GENERAL, |channel| channel.id);
        let name_for = |member: MemberId| {
            snapshot
                .display_names()
                .get(&member)
                .map(|name| name.as_str().to_owned())
                .unwrap_or_else(|| short_member(member))
        };
        let messages = snapshot
            .messages()
            .iter()
            .map(|authored| MessageState {
                id: authored.message().id(),
                channel: authored.message().channel_id(),
                author_id: authored.author(),
                author: name_for(authored.author()),
                body: authored.message().body().to_owned(),
            })
            .collect();
        let attachments = snapshot
            .attachments()
            .iter()
            .map(|authored| AttachmentState {
                id: authored.attachment().id(),
                channel: authored.attachment().channel_id(),
                author_id: authored.author(),
                author: name_for(authored.author()),
                name: authored.attachment().name().to_owned(),
                bytes: authored.attachment().bytes().to_vec(),
            })
            .collect();
        let members = snapshot
            .members()
            .iter()
            .copied()
            .map(|member| MemberState {
                id: member,
                name: name_for(member),
                is_owner: member == snapshot.owner(),
                is_local: member == local_member,
                role: snapshot.role(member).unwrap_or(MemberRole::Participant),
            })
            .collect();

        Self {
            channels,
            messages,
            attachments,
            members,
            selected_channel,
            owner: snapshot.owner(),
            local_member,
            access,
            online: BTreeSet::new(),
            pending_members: BTreeSet::new(),
            voice: BTreeMap::new(),
            speaking: BTreeMap::new(),
            last_read: BTreeMap::new(),
            local_address: String::new(),
            connection: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn replace_snapshot(&mut self, snapshot: &Snapshot) {
        let previous_access = self.access;
        let selected = self.selected_channel;
        let online = std::mem::take(&mut self.online);
        let pending_members = std::mem::take(&mut self.pending_members);
        let voice = std::mem::take(&mut self.voice);
        let speaking = std::mem::take(&mut self.speaking);
        let last_read = std::mem::take(&mut self.last_read);
        let local_address = std::mem::take(&mut self.local_address);
        let connection = self.connection;
        let diagnostics = std::mem::take(&mut self.diagnostics);
        *self = Self::from_snapshot(snapshot, self.local_member);
        self.access = next_access(
            previous_access,
            snapshot.members().contains(&self.local_member),
        );
        self.online = online;
        self.online
            .retain(|member| snapshot.members().contains(member));
        self.pending_members = pending_members;
        self.pending_members
            .retain(|member| !snapshot.members().contains(member));
        self.voice = voice;
        self.speaking = speaking;
        self.last_read = last_read;
        self.local_address = local_address;
        self.connection = connection;
        self.diagnostics = diagnostics;
        for members in self.voice.values_mut() {
            members.retain(|member, _| snapshot.members().contains(member));
        }
        self.voice.retain(|channel, members| {
            !members.is_empty() && self.channels.iter().any(|stored| stored.id == *channel)
        });
        self.speaking.retain(|(channel, member), _| {
            snapshot.members().contains(member)
                && self.channels.iter().any(|stored| stored.id == *channel)
        });
        self.select_channel(selected);
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::DisplayNameChanged { member, name } => {
                let name = name.as_str();
                if let Some(stored) = self.members.iter_mut().find(|stored| stored.id == member) {
                    stored.name = name.to_owned();
                }
                for message in self
                    .messages
                    .iter_mut()
                    .filter(|message| message.author_id == member)
                {
                    message.author = name.to_owned();
                }
                for attachment in self
                    .attachments
                    .iter_mut()
                    .filter(|attachment| attachment.author_id == member)
                {
                    attachment.author = name.to_owned();
                }
            }
            Event::ChannelCreated(channel) => {
                if !self.channels.iter().any(|stored| stored.id == channel.id()) {
                    self.channels.push(ChannelState {
                        id: channel.id(),
                        name: channel.name().to_owned(),
                        kind: channel.kind(),
                    });
                }
            }
            Event::TextStored(authored) => {
                let message = authored.message();
                if !self.messages.iter().any(|stored| {
                    stored.author_id == authored.author() && stored.id == message.id()
                }) {
                    let author = self
                        .members
                        .iter()
                        .find(|member| member.id == authored.author())
                        .map_or_else(
                            || short_member(authored.author()),
                            |member| member.name.clone(),
                        );
                    self.messages.push(MessageState {
                        id: message.id(),
                        channel: message.channel_id(),
                        author_id: authored.author(),
                        author,
                        body: message.body().to_owned(),
                    });
                    self.messages.sort_by_key(|message| message.id);
                }
            }
            Event::AttachmentStored(authored) => {
                let attachment = authored.attachment();
                if !self.attachments.iter().any(|stored| {
                    stored.author_id == authored.author() && stored.id == attachment.id()
                }) {
                    let author = self
                        .members
                        .iter()
                        .find(|member| member.id == authored.author())
                        .map_or_else(
                            || short_member(authored.author()),
                            |member| member.name.clone(),
                        );
                    self.attachments.push(AttachmentState {
                        id: attachment.id(),
                        channel: attachment.channel_id(),
                        author_id: authored.author(),
                        author,
                        name: attachment.name().to_owned(),
                        bytes: attachment.bytes().to_vec(),
                    });
                    self.attachments.sort_by_key(|attachment| attachment.id);
                }
            }
            Event::AttachmentForgotten { author, id } => {
                self.attachments
                    .retain(|stored| stored.author_id != author || stored.id != id);
            }
            Event::PeerConnected(member) => {
                if self.local_member == self.owner
                    && !self.members.iter().any(|stored| stored.id == member)
                {
                    self.pending_members.insert(member);
                } else {
                    self.online.insert(member);
                }
            }
            Event::MembershipChanged(community) => {
                self.owner = community.owner();
                let names = self
                    .members
                    .iter()
                    .map(|member| (member.id, member.name.clone()))
                    .collect::<BTreeMap<_, _>>();
                self.members = community
                    .members()
                    .map(|member| MemberState {
                        id: member,
                        name: names
                            .get(&member)
                            .cloned()
                            .unwrap_or_else(|| short_member(member)),
                        is_owner: member == self.owner,
                        is_local: member == self.local_member,
                        role: community.role(member).unwrap_or(MemberRole::Participant),
                    })
                    .collect();
                self.access = next_access(self.access, community.contains(self.local_member));
                self.online.retain(|member| community.contains(*member));
                self.pending_members
                    .retain(|member| !community.contains(*member));
                for members in self.voice.values_mut() {
                    members.retain(|member, _| community.contains(*member));
                }
                self.speaking
                    .retain(|(_, member), _| community.contains(*member));
                self.voice.retain(|_, members| !members.is_empty());
            }
            Event::VoiceReceived(authored) => {
                self.speaking.insert(
                    (authored.frame().channel_id(), authored.author()),
                    Instant::now(),
                );
            }
            Event::VoicePresence {
                channel,
                member,
                state,
            } => match state {
                VoicePresence::Joined => {
                    for members in self.voice.values_mut() {
                        members.remove(&member);
                    }
                    self.voice.entry(channel).or_default().insert(member, false);
                }
                VoicePresence::Left => {
                    if let Some(members) = self.voice.get_mut(&channel) {
                        members.remove(&member);
                    }
                    self.voice.retain(|_, members| !members.is_empty());
                    self.speaking.remove(&(channel, member));
                }
                VoicePresence::Muted(muted) => {
                    if let Some(member) = self
                        .voice
                        .get_mut(&channel)
                        .and_then(|members| members.get_mut(&member))
                    {
                        *member = muted;
                    }
                }
            },
            _ => {}
        }
    }

    pub fn select_channel(&mut self, channel: ChannelId) {
        if self.channels.iter().any(|stored| stored.id == channel) {
            self.selected_channel = channel;
        }
    }

    pub fn voice_members(&self, channel: ChannelId) -> Vec<(MemberState, bool)> {
        self.voice
            .get(&channel)
            .into_iter()
            .flatten()
            .filter_map(|(member, muted)| {
                self.members
                    .iter()
                    .find(|stored| stored.id == *member)
                    .cloned()
                    .map(|member| (member, *muted))
            })
            .collect()
    }

    pub fn is_speaking(&self, channel: ChannelId, member: MemberId) -> bool {
        self.speaking
            .get(&(channel, member))
            .is_some_and(|last| speaking_recent(last.elapsed()))
    }

    pub fn is_online(&self, member: MemberId) -> bool {
        member == self.local_member || self.online.contains(&member)
    }

    pub const fn access(&self) -> CommunityAccess {
        self.access
    }

    pub const fn local_member(&self) -> MemberId {
        self.local_member
    }

    pub fn pending_members(&self) -> impl Iterator<Item = MemberId> + '_ {
        self.pending_members.iter().copied()
    }

    pub fn dismiss_pending(&mut self, member: MemberId) {
        self.pending_members.remove(&member);
    }

    pub fn latest_item(&self, channel: ChannelId) -> Option<MessageId> {
        self.messages
            .iter()
            .filter(|message| message.channel == channel)
            .map(|message| message.id)
            .chain(
                self.attachments
                    .iter()
                    .filter(|attachment| attachment.channel == channel)
                    .map(|attachment| attachment.id),
            )
            .max()
    }

    pub fn record_read(&mut self, channel: ChannelId, message: MessageId) {
        self.last_read
            .entry(channel)
            .and_modify(|stored| *stored = (*stored).max(message))
            .or_insert(message);
    }

    pub fn unread_count(&self, channel: ChannelId) -> usize {
        let last_read = self.last_read.get(&channel).copied();
        self.messages
            .iter()
            .filter(|message| {
                message.channel == channel
                    && is_unread(last_read, self.local_member, message.author_id, message.id)
            })
            .count()
            + self
                .attachments
                .iter()
                .filter(|attachment| {
                    attachment.channel == channel
                        && is_unread(
                            last_read,
                            self.local_member,
                            attachment.author_id,
                            attachment.id,
                        )
                })
                .count()
    }

    pub fn set_local_address(&mut self, address: String) {
        self.local_address = address;
    }

    pub fn local_address(&self) -> &str {
        &self.local_address
    }

    pub fn apply_diagnostics(&mut self, peers: &[PeerDiagnostic]) {
        self.diagnostics = peers.to_vec();
        self.online = peers
            .iter()
            .map(PeerDiagnostic::member)
            .filter(|member| self.members.iter().any(|stored| stored.id == *member))
            .collect();
        self.connection = peers
            .iter()
            .filter(|peer| self.members.iter().any(|stored| stored.id == peer.member()))
            .flat_map(PeerDiagnostic::paths)
            .filter(|path| path.is_selected())
            .map(|path| (path.kind(), path.rtt().as_millis() as u64))
            .max_by_key(|(kind, rtt)| (path_priority(*kind), *rtt));
    }

    pub const fn connection(&self) -> Option<(ConnectionPathKind, u64)> {
        self.connection
    }

    pub fn peer_paths(&self, member: MemberId) -> Vec<(ConnectionPathKind, bool, u64)> {
        self.diagnostics
            .iter()
            .find(|peer| peer.member() == member)
            .into_iter()
            .flat_map(PeerDiagnostic::paths)
            .map(|path| {
                (
                    path.kind(),
                    path.is_selected(),
                    path.rtt().as_millis() as u64,
                )
            })
            .collect()
    }

    pub fn online_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| self.is_online(member.id))
            .count()
    }
}

fn initial_access(has_history: bool, admitted: bool) -> CommunityAccess {
    if admitted {
        CommunityAccess::Active
    } else if has_history {
        CommunityAccess::Removed
    } else {
        CommunityAccess::Waiting
    }
}

fn next_access(previous: CommunityAccess, admitted: bool) -> CommunityAccess {
    if admitted {
        CommunityAccess::Active
    } else if previous == CommunityAccess::Waiting {
        CommunityAccess::Waiting
    } else {
        CommunityAccess::Removed
    }
}

fn speaking_recent(elapsed: Duration) -> bool {
    elapsed < Duration::from_millis(700)
}

pub fn connection_label(connection: Option<(ConnectionPathKind, u64)>) -> String {
    match connection {
        Some((ConnectionPathKind::Direct, rtt)) => format!("DIRECT {rtt}ms"),
        Some((ConnectionPathKind::Relay, rtt)) => format!("RELAY {rtt}ms"),
        Some((ConnectionPathKind::Custom, rtt)) => format!("CUSTOM {rtt}ms"),
        None => "OFFLINE".to_owned(),
    }
}

pub const fn voice_room_available(occupancy: usize, local_is_present: bool) -> bool {
    occupancy < MAX_VOICE_PARTICIPANTS || local_is_present
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectivityMode {
    #[default]
    Wan,
    RelayOnly,
}

impl ConnectivityMode {
    fn configure(self, config: NodeConfig) -> NodeConfig {
        match self {
            Self::Wan => config.wan(),
            Self::RelayOnly => config.relay_only(),
        }
    }
}

const fn path_priority(kind: ConnectionPathKind) -> u8 {
    match kind {
        ConnectionPathKind::Direct => 0,
        ConnectionPathKind::Relay => 1,
        ConnectionPathKind::Custom => 2,
    }
}

fn is_unread(
    last_read: Option<MessageId>,
    local_member: MemberId,
    author: MemberId,
    id: MessageId,
) -> bool {
    author != local_member && Some(id) > last_read
}

pub fn open_existing(
    data_dir: PathBuf,
    connectivity: ConnectivityMode,
) -> Result<(CommunityState, Session), String> {
    if !data_dir.join("peer.db").is_file() {
        return Err(format!(
            "{} is not an existing community",
            data_dir.display()
        ));
    }
    open(
        connectivity.configure(NodeConfig::new(data_dir).existing()),
        None,
    )
}

pub fn create_new(
    data_dir: PathBuf,
    connectivity: ConnectivityMode,
) -> Result<(CommunityState, Session), String> {
    open(connectivity.configure(NodeConfig::new(data_dir)), None)
}

pub fn join_new(
    data_dir: PathBuf,
    invite: CommunityInvite,
    connectivity: ConnectivityMode,
) -> Result<(CommunityState, Session), String> {
    let address = invite.owner_address().clone();
    open(
        connectivity.configure(
            NodeConfig::new(data_dir).community(invite.community_id(), address.member_id()),
        ),
        Some(address),
    )
}

pub fn recover(
    data_dir: PathBuf,
    backup: PathBuf,
    passphrase: &str,
    connectivity: ConnectivityMode,
) -> Result<(CommunityState, Session), String> {
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .block_on(restore_identity(&data_dir, backup, passphrase))
        .map_err(|error| error.to_string())?;
    drop(runtime);
    open(
        connectivity.configure(NodeConfig::new(data_dir).existing()),
        None,
    )
}

fn open(
    config: NodeConfig,
    connect: Option<PeerAddress>,
) -> Result<(CommunityState, Session), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let node = Arc::new(
        runtime
            .block_on(Node::open(config))
            .map_err(|error| error.to_string())?,
    );
    if let Some(address) = connect {
        runtime
            .block_on(node.connect(address))
            .map_err(|error| error.to_string())?;
    }
    let mut events = node.subscribe();
    let snapshot = runtime
        .block_on(node.snapshot())
        .map_err(|error| error.to_string())?;
    let mut state = CommunityState::from_snapshot(&snapshot, node.member_id());
    state.set_local_address(node.address().to_string());
    for channel in state
        .channels
        .iter()
        .filter(|channel| channel.kind == ChannelKind::Text)
        .map(|channel| channel.id)
        .collect::<Vec<_>>()
    {
        if let Some(message) = runtime
            .block_on(node.last_read(channel))
            .map_err(|error| error.to_string())?
        {
            state.record_read(channel, message);
        }
    }
    let (commands, mut command_rx) = tokio_mpsc::unbounded_channel();
    let internal_commands = commands.clone();
    let (update_tx, update_rx) = tokio_mpsc::unbounded_channel();
    let updates = SessionUpdates(Arc::new(tokio::sync::Mutex::new(update_rx)));
    let command_node = node.clone();
    let command_updates = update_tx.clone();
    let command_task = runtime.spawn(async move {
        let mut voice: Option<(u64, ChannelId, VoiceSession)> = None;
        let mut voice_generation = 0_u64;
        let mut voice_devices = VoiceDeviceConfig::default();
        while let Some(command) = command_rx.recv().await {
            match command {
                SessionCommand::Execute(command) => {
                    if let Err(error) = command_node.execute(command).await {
                        let _ =
                            command_updates.send(SessionUpdate::CommandFailed(error.to_string()));
                    }
                }
                SessionCommand::MarkRead { channel, message } => {
                    match command_node.set_last_read(channel, message).await {
                        Ok(()) => {
                            let _ = command_updates
                                .send(SessionUpdate::ReadStored { channel, message });
                        }
                        Err(error) => {
                            let _ = command_updates
                                .send(SessionUpdate::CommandFailed(error.to_string()));
                        }
                    }
                }
                SessionCommand::Connect(address) => {
                    if let Err(error) = command_node.connect(address).await {
                        let _ =
                            command_updates.send(SessionUpdate::CommandFailed(error.to_string()));
                    }
                }
                SessionCommand::RequestInvite => match command_node.community_invite().await {
                    Ok(invite) => {
                        let _ =
                            command_updates.send(SessionUpdate::InviteReady(invite.to_string()));
                    }
                    Err(error) => {
                        let _ =
                            command_updates.send(SessionUpdate::CommandFailed(error.to_string()));
                    }
                },
                SessionCommand::ExportIdentity { path, passphrase } => {
                    if let Err(error) = command_node.export_identity(path, &passphrase).await {
                        let _ =
                            command_updates.send(SessionUpdate::CommandFailed(error.to_string()));
                    }
                }
                SessionCommand::JoinVoice(channel) => {
                    voice_generation = voice_generation.wrapping_add(1);
                    if let Some((_, previous, session)) = voice.take() {
                        let _ = command_updates
                            .send(SessionUpdate::Voice(VoiceUpdate::Leaving(previous)));
                        if let Err(error) = session.leave().await {
                            let _ =
                                command_updates.send(SessionUpdate::Voice(VoiceUpdate::Failed {
                                    channel: previous,
                                    error: error.to_string(),
                                }));
                        }
                    }
                    let _ =
                        command_updates.send(SessionUpdate::Voice(VoiceUpdate::Joining(channel)));
                    match VoiceSession::join_channel_with_config(
                        command_node.clone(),
                        channel,
                        voice_devices.clone(),
                    )
                    .await
                    {
                        Ok(session) => {
                            let completion = session.completion();
                            let generation = voice_generation;
                            let completed_commands = internal_commands.clone();
                            tokio::spawn(async move {
                                let result =
                                    completion.wait().await.map_err(|error| error.to_string());
                                let _ = completed_commands.send(SessionCommand::VoiceFinished {
                                    generation,
                                    channel,
                                    result,
                                });
                            });
                            voice = Some((generation, channel, session));
                            let _ = command_updates
                                .send(SessionUpdate::Voice(VoiceUpdate::Joined(channel)));
                        }
                        Err(error) => {
                            let _ =
                                command_updates.send(SessionUpdate::Voice(VoiceUpdate::Failed {
                                    channel,
                                    error: error.to_string(),
                                }));
                        }
                    }
                }
                SessionCommand::SetVoiceMuted(muted) => {
                    if let Some((_, _, session)) = &voice {
                        session.set_muted(muted);
                        let _ =
                            command_updates.send(SessionUpdate::Voice(VoiceUpdate::Muted(muted)));
                    }
                }
                SessionCommand::SetVoiceDeafened(deafened) => {
                    if let Some((_, _, session)) = &voice {
                        session.set_deafened(deafened);
                        let _ = command_updates
                            .send(SessionUpdate::Voice(VoiceUpdate::Deafened(deafened)));
                    }
                }
                SessionCommand::SetVoiceDevices(devices) => voice_devices = devices,
                SessionCommand::LeaveVoice => {
                    voice_generation = voice_generation.wrapping_add(1);
                    if let Some((_, channel, session)) = voice.take() {
                        let _ = command_updates
                            .send(SessionUpdate::Voice(VoiceUpdate::Leaving(channel)));
                        match session.leave().await {
                            Ok(()) => {
                                let _ = command_updates
                                    .send(SessionUpdate::Voice(VoiceUpdate::Disconnected));
                            }
                            Err(error) => {
                                let _ = command_updates.send(SessionUpdate::Voice(
                                    VoiceUpdate::Failed {
                                        channel,
                                        error: error.to_string(),
                                    },
                                ));
                            }
                        }
                    }
                }
                SessionCommand::VoiceFinished {
                    generation,
                    channel,
                    result,
                } => {
                    if voice
                        .as_ref()
                        .is_some_and(|(current, _, _)| *current == generation)
                    {
                        voice = None;
                        let update = match result {
                            Ok(()) => VoiceUpdate::Disconnected,
                            Err(error) => VoiceUpdate::Failed { channel, error },
                        };
                        let _ = command_updates.send(SessionUpdate::Voice(update));
                    }
                }
                SessionCommand::Shutdown(stopped) => {
                    if let Some((_, _, session)) = voice.take() {
                        let _ = session.leave().await;
                    }
                    let _ = stopped.send(());
                    break;
                }
            }
        }
    });
    let event_node = node.clone();
    let event_task = runtime.spawn(async move {
        let mut diagnostics = tokio::time::interval(std::time::Duration::from_secs(1));
        diagnostics.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        diagnostics.tick().await;
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => {
                        if update_tx.send(SessionUpdate::Event(event)).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => match event_node.snapshot().await {
                        Ok(snapshot) => {
                            if update_tx.send(SessionUpdate::Snapshot(snapshot)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            if update_tx
                                .send(SessionUpdate::CommandFailed(error.to_string()))
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = diagnostics.tick() => {
                    let peers = event_node.connection_diagnostics().await;
                    if update_tx.send(SessionUpdate::Diagnostics(peers)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok((
        state,
        Session {
            node: Some(node),
            runtime: Some(runtime),
            commands,
            updates,
            command_task: Some(command_task),
            event_task: Some(event_task),
        },
    ))
}

fn short_member(member: MemberId) -> String {
    member
        .as_bytes()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use grimoire_core::{
        Attachment, Channel, ChannelId, ChannelKind, Command, DisplayName, MemberId, MessageId,
        Node, NodeConfig, TextMessage,
    };

    use super::{
        CommunityAccess, CommunityState, ConnectivityMode, SessionUpdate, VoiceState, VoiceUpdate,
        connection_label, initial_access, is_unread, next_access, open_existing, speaking_recent,
        voice_room_available,
    };

    #[test]
    fn access_state_tracks_waiting_admission_and_removal() {
        assert_eq!(initial_access(false, false), CommunityAccess::Waiting);
        assert_eq!(initial_access(true, false), CommunityAccess::Removed);
        assert_eq!(initial_access(true, true), CommunityAccess::Active);
        assert_eq!(
            next_access(CommunityAccess::Waiting, false),
            CommunityAccess::Waiting
        );
        assert_eq!(
            next_access(CommunityAccess::Waiting, true),
            CommunityAccess::Active
        );
        assert_eq!(
            next_access(CommunityAccess::Active, false),
            CommunityAccess::Removed
        );
        assert_eq!(
            next_access(CommunityAccess::Removed, false),
            CommunityAccess::Removed
        );
    }

    #[test]
    fn full_voice_room_only_allows_existing_participants() {
        assert!(voice_room_available(3, false));
        assert!(!voice_room_available(4, false));
        assert!(voice_room_available(4, true));
    }

    #[test]
    fn speaking_expires_after_recency_window() {
        assert!(speaking_recent(Duration::from_millis(0)));
        assert!(speaking_recent(Duration::from_millis(699)));
        assert!(!speaking_recent(Duration::from_millis(700)));
    }

    #[test]
    fn connection_labels_describe_selected_path() {
        assert_eq!(connection_label(None), "OFFLINE");
        assert_eq!(
            connection_label(Some((grimoire_core::ConnectionPathKind::Direct, 42))),
            "DIRECT 42ms"
        );
        assert_eq!(
            connection_label(Some((grimoire_core::ConnectionPathKind::Relay, 180))),
            "RELAY 180ms"
        );
        assert_eq!(
            connection_label(Some((grimoire_core::ConnectionPathKind::Custom, 90))),
            "CUSTOM 90ms"
        );
    }

    #[test]
    fn unread_logic_excludes_local_and_seen_items() {
        let local = MemberId::from_bytes([1; 32]);
        let remote = MemberId::from_bytes([2; 32]);
        let seen = MessageId::from_bytes([3; 32]);
        let newer = MessageId::from_bytes([4; 32]);

        assert!(!is_unread(Some(seen), local, local, newer));
        assert!(!is_unread(Some(seen), local, remote, seen));
        assert!(is_unread(Some(seen), local, remote, newer));
        assert!(is_unread(None, local, remote, seen));
    }

    #[tokio::test]
    async fn snapshot_projection_exposes_real_chat_data() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
        node.execute(Command::SetDisplayName(DisplayName::new("maren").unwrap()))
            .await
            .unwrap();
        node.execute(Command::PostText(
            TextMessage::new(MessageId::generate(), "hello from gpui").unwrap(),
        ))
        .await
        .unwrap();

        let state =
            CommunityState::from_snapshot(&node.snapshot().await.unwrap(), node.member_id());

        assert_eq!(state.channels[0].name, "general");
        assert_eq!(state.messages[0].author, "maren");
        assert_eq!(state.messages[0].body, "hello from gpui");
        assert_eq!(state.members[0].name, "maren");
        assert!(state.members[0].is_owner);
        assert!(state.members[0].is_local);
    }

    #[tokio::test]
    async fn node_events_update_one_community_projection() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
        let mut events = node.subscribe();
        let mut state =
            CommunityState::from_snapshot(&node.snapshot().await.unwrap(), node.member_id());
        let channel = Channel::new(
            ChannelId::from_bytes([7; 32]),
            "field-notes",
            ChannelKind::Text,
        )
        .unwrap();

        node.execute(Command::SetDisplayName(DisplayName::new("maren").unwrap()))
            .await
            .unwrap();
        state.apply(events.recv().await.unwrap());
        node.execute(Command::CreateChannel(channel.clone()))
            .await
            .unwrap();
        state.apply(events.recv().await.unwrap());
        node.execute(Command::ShareAttachment(
            Attachment::new(
                MessageId::generate(),
                channel.id(),
                "signal.txt",
                b"signal".to_vec(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
        state.apply(events.recv().await.unwrap());
        node.execute(Command::PostText(
            TextMessage::in_channel(MessageId::generate(), channel.id(), "signal").unwrap(),
        ))
        .await
        .unwrap();
        state.apply(events.recv().await.unwrap());

        assert_eq!(state.members[0].name, "maren");
        assert!(
            state
                .channels
                .iter()
                .any(|stored| stored.id == channel.id())
        );
        assert_eq!(state.messages.last().unwrap().body, "signal");
        assert_eq!(state.attachments.last().unwrap().name, "signal.txt");
    }

    #[tokio::test]
    async fn snapshot_refresh_preserves_passive_presence() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
        let snapshot = node.snapshot().await.unwrap();
        let member = node.member_id();
        let pending = grimoire_core::MemberId::from_bytes([8; 32]);
        let channel = ChannelId::GENERAL;
        let mut state = CommunityState::from_snapshot(&snapshot, member);

        state.apply(grimoire_core::Event::PeerConnected(pending));
        state.apply(grimoire_core::Event::PeerConnected(member));
        state.apply(grimoire_core::Event::VoicePresence {
            channel,
            member,
            state: grimoire_core::VoicePresence::Joined,
        });
        state.apply(grimoire_core::Event::VoicePresence {
            channel,
            member,
            state: grimoire_core::VoicePresence::Muted(true),
        });
        state.replace_snapshot(&snapshot);

        assert!(state.is_online(member));
        assert_eq!(state.pending_members().collect::<Vec<_>>(), vec![pending]);
        assert_eq!(
            state.voice_members(channel),
            vec![(state.members[0].clone(), true)]
        );
    }

    #[test]
    fn session_orders_commands_and_shuts_down_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            Node::open(NodeConfig::new(directory.path()))
                .await
                .unwrap()
                .shutdown()
                .await
                .unwrap();
        });
        drop(runtime);

        let (_, session) =
            open_existing(directory.path().to_owned(), ConnectivityMode::Wan).unwrap();
        session
            .execute(Command::SetDisplayName(DisplayName::new("first").unwrap()))
            .unwrap();
        session
            .execute(Command::SetDisplayName(DisplayName::new("second").unwrap()))
            .unwrap();

        let names = (0..2)
            .map(
                |_| match session.recv_timeout(Duration::from_secs(2)).unwrap() {
                    SessionUpdate::Event(grimoire_core::Event::DisplayNameChanged {
                        name, ..
                    }) => name.as_str().to_owned(),
                    update => panic!("unexpected update: {update:?}"),
                },
            )
            .collect::<Vec<_>>();

        assert_eq!(names, ["first", "second"]);
        session.close().unwrap();
    }

    #[test]
    fn voice_state_survives_channel_navigation_until_disconnect() {
        let channel = ChannelId::from_bytes([9; 32]);
        let mut voice = VoiceState::default();

        voice.apply(VoiceUpdate::Joining(channel));
        voice.apply(VoiceUpdate::Joined(channel));
        voice.apply(VoiceUpdate::Muted(true));
        voice.apply(VoiceUpdate::Deafened(true));

        assert_eq!(voice.channel(), Some(channel));
        assert!(voice.is_connected());
        assert!(voice.is_muted());
        assert!(voice.is_deafened());

        voice.apply(VoiceUpdate::Leaving(channel));
        assert_eq!(voice.channel(), Some(channel));
        voice.apply(VoiceUpdate::Disconnected);
        assert_eq!(voice.channel(), None);
        assert!(!voice.is_deafened());
    }
}
