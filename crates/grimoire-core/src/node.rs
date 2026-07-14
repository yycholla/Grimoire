use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use ed25519_dalek::SigningKey;
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use prost::Message;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast, oneshot};

use crate::{
    AttachmentError, AuthoredVoice, Channel, ChannelError, ChannelId, ChannelKind, Command,
    Community, CommunityError, CommunityId, DisplayName, DisplayNameError, Event, FaultKind,
    MemberId, MemberRole, MembershipChange, MessageError, MessageId, NodeError, Snapshot,
    TextMessage, VoiceFrame, VoiceFrameError, VoicePresence, VoiceStreamId,
    community::{MembershipUpdate, SignedMembership},
    crypto::{
        ContentEpoch, ContentKeyEnvelope, EncryptedAttachment, EncryptedMemberProfile,
        EncryptedText, EncryptedVoicePresence, KeyRegistration, decrypt_voice, encrypt_voice,
        hpke_public,
    },
    metrics::{Metrics, MetricsSnapshot},
    model::{
        MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_NAME_BYTES, MAX_VOICE_PARTICIPANTS,
        SignedCreateChannel,
    },
    store::Store,
    wire,
};

const ALPN: &[u8] = b"peer-community/operations/2";
const MAX_OPERATION_BYTES: usize = MAX_ATTACHMENT_BYTES + 2048;
const MAX_PENDING_REGISTRATIONS: usize = 64;
const MAX_VOICE_PRESENCE_CIPHERTEXT: usize = 128;
const RECONNECT_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveVoicePresence {
    channel: ChannelId,
    muted: bool,
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    data_dir: PathBuf,
    community: Option<(CommunityId, MemberId)>,
    connectivity: Connectivity,
    require_existing_community: bool,
}

#[derive(Clone, Copy, Debug, Default)]
enum Connectivity {
    #[default]
    Local,
    Wan,
    RelayOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionPathKind {
    Direct,
    Relay,
    Custom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionPathDiagnostic {
    kind: ConnectionPathKind,
    selected: bool,
    rtt: std::time::Duration,
}

impl ConnectionPathDiagnostic {
    pub const fn kind(&self) -> ConnectionPathKind {
        self.kind
    }

    pub const fn is_selected(&self) -> bool {
        self.selected
    }

    pub const fn rtt(&self) -> std::time::Duration {
        self.rtt
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerDiagnostic {
    member: MemberId,
    paths: Vec<ConnectionPathDiagnostic>,
}

impl PeerDiagnostic {
    pub const fn member(&self) -> MemberId {
        self.member
    }

    pub fn paths(&self) -> &[ConnectionPathDiagnostic] {
        &self.paths
    }
}

impl NodeConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            community: None,
            connectivity: Connectivity::Local,
            require_existing_community: false,
        }
    }

    pub fn community_owner(mut self, owner: MemberId) -> Self {
        self.community = Some((CommunityId::legacy(owner), owner));
        self
    }

    pub fn community(mut self, id: CommunityId, owner: MemberId) -> Self {
        self.community = Some((id, owner));
        self
    }

    pub fn wan(mut self) -> Self {
        self.connectivity = Connectivity::Wan;
        self
    }

    pub fn relay_only(mut self) -> Self {
        self.connectivity = Connectivity::RelayOnly;
        self
    }

    pub fn existing(mut self) -> Self {
        self.require_existing_community = true;
        self
    }
}

impl From<turso::Error> for NodeError {
    fn from(error: turso::Error) -> Self {
        Self::new(FaultKind::Storage, error)
    }
}

impl From<std::io::Error> for NodeError {
    fn from(error: std::io::Error) -> Self {
        Self::new(FaultKind::Filesystem, error)
    }
}

impl From<prost::DecodeError> for NodeError {
    fn from(error: prost::DecodeError) -> Self {
        Self::protocol(error)
    }
}

impl From<MessageError> for NodeError {
    fn from(error: MessageError) -> Self {
        Self::new(FaultKind::InvalidMessage, error)
    }
}

impl From<AttachmentError> for NodeError {
    fn from(error: AttachmentError) -> Self {
        Self::new(FaultKind::InvalidMessage, error)
    }
}

impl From<ChannelError> for NodeError {
    fn from(error: ChannelError) -> Self {
        Self::new(FaultKind::InvalidMessage, error)
    }
}

impl From<DisplayNameError> for NodeError {
    fn from(error: DisplayNameError) -> Self {
        Self::new(FaultKind::InvalidMessage, error)
    }
}

impl From<VoiceFrameError> for NodeError {
    fn from(error: VoiceFrameError) -> Self {
        Self::protocol(error)
    }
}

impl From<CommunityError> for NodeError {
    fn from(error: CommunityError) -> Self {
        Self::authorization(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerAddress(EndpointAddr);

impl PeerAddress {
    pub fn member_id(&self) -> MemberId {
        MemberId::from_bytes(*self.0.id.as_bytes())
    }
}

impl fmt::Display for PeerAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = serde_json::to_string(&self.0).map_err(|_| fmt::Error)?;
        formatter.write_str(&encoded)
    }
}

impl FromStr for PeerAddress {
    type Err = PeerAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
            .map(Self)
            .map_err(|error| PeerAddressError(error.to_string()))
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid peer address: {0}")]
pub struct PeerAddressError(String);

impl PeerAddressError {
    pub fn message(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommunityInvite {
    community_id: CommunityId,
    owner_address: PeerAddress,
}

impl CommunityInvite {
    pub const fn community_id(&self) -> CommunityId {
        self.community_id
    }

    pub const fn owner_address(&self) -> &PeerAddress {
        &self.owner_address
    }
}

impl fmt::Display for CommunityInvite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("pc1:")?;
        for byte in self.community_id.as_bytes() {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, ":{}", self.owner_address)
    }
}

impl FromStr for CommunityInvite {
    type Err = CommunityInviteError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value
            .strip_prefix("pc1:")
            .ok_or_else(|| CommunityInviteError("missing pc1 prefix".into()))?;
        let (community, address) = value
            .split_once(':')
            .ok_or_else(|| CommunityInviteError("missing owner address".into()))?;
        if community.len() != 64 {
            return Err(CommunityInviteError("community id is not 32 bytes".into()));
        }
        let mut id = [0; 32];
        for (index, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&community[index * 2..index * 2 + 2], 16)
                .map_err(|error| CommunityInviteError(error.to_string()))?;
        }
        Ok(Self {
            community_id: CommunityId::from_bytes(id),
            owner_address: address
                .parse()
                .map_err(|error: PeerAddressError| CommunityInviteError(error.to_string()))?,
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid community invite: {0}")]
pub struct CommunityInviteError(String);

pub struct Node {
    router: Router,
    handler: OperationHandler,
    signing_key: SigningKey,
    community_id: CommunityId,
    store: Arc<Store>,
    connections: Arc<Mutex<Vec<PeerConnection>>>,
    community: Arc<RwLock<Community>>,
    metrics: Arc<Metrics>,
    voice_presence: Arc<Mutex<BTreeMap<MemberId, ActiveVoicePresence>>>,
    voice_presence_commands: Arc<Mutex<()>>,
    events: broadcast::Sender<Event>,
    reconnect_shutdown: Option<oneshot::Sender<()>>,
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
}

impl Node {
    pub async fn open(config: NodeConfig) -> Result<Self, NodeError> {
        let store = Arc::new(Store::open(&config.data_dir).await?);
        let metrics = Arc::new(Metrics::default());
        if config.require_existing_community {
            store.community_id().await?;
        }
        let identity_seed = store.identity_seed().await?;
        let signing_key = SigningKey::from_bytes(&identity_seed);
        let local_member = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        store
            .initialize_community(config.community, local_member)
            .await?;
        let community_id = store.community_id().await?;
        let hpke_seed = store.hpke_seed().await?;
        let registration =
            KeyRegistration::sign(&signing_key, community_id, hpke_public(&hpke_seed));
        store.insert_key_registration(&registration).await?;
        if store.community_owner().await? == local_member {
            for channel in Channel::default_channels() {
                store
                    .insert_channel(&SignedCreateChannel::sign(
                        &signing_key,
                        community_id,
                        channel,
                    ))
                    .await?;
            }
        }
        let community = Arc::new(RwLock::new(store.community().await?));
        if store.community_owner().await? == local_member
            && store.active_content_epoch().await?.is_none()
            && !store.is_recovered().await?
        {
            let (number, head) = store.active_membership_head().await?;
            let epoch = ContentEpoch {
                number,
                head,
                key: rand::random(),
            };
            store.insert_content_key(epoch).await?;
            let envelope =
                ContentKeyEnvelope::seal(&signing_key, community_id, epoch, &registration)?;
            store.insert_content_key_envelope(&envelope).await?;
        }
        let connections = Arc::new(Mutex::new(Vec::new()));
        let pending_registrations = Arc::new(Mutex::new(BTreeMap::new()));
        let membership_commands = Arc::new(Mutex::new(()));
        let profile_commands = Arc::new(Mutex::new(()));
        let voice_presence = Arc::new(Mutex::new(BTreeMap::new()));
        let voice_presence_commands = Arc::new(Mutex::new(()));
        let voice_presence_receives = Arc::new(Mutex::new(()));
        let (events, _) = broadcast::channel(64);
        let egregore = Arc::new(Egregore {
            store: store.clone(),
            community: community.clone(),
            local_member,
            community_id,
            hpke_seed,
            signing_key: signing_key.clone(),
            pending_registrations: pending_registrations.clone(),
            membership_commands,
            profile_commands: profile_commands.clone(),
            metrics: metrics.clone(),
        });
        let endpoint = match config.connectivity {
            Connectivity::Local => Endpoint::builder(presets::Minimal),
            Connectivity::Wan => Endpoint::builder(presets::N0),
            Connectivity::RelayOnly => Endpoint::builder(presets::N0).clear_ip_transports(),
        }
        .secret_key(iroh::SecretKey::from_bytes(&identity_seed))
        .bind()
        .await
        .map_err(NodeError::network)?;
        if matches!(config.connectivity, Connectivity::RelayOnly) {
            tokio::time::timeout(std::time::Duration::from_secs(15), endpoint.online())
                .await
                .map_err(|_| NodeError::network("relay did not become ready within 15 seconds"))?;
        }
        let handler = OperationHandler {
            egregore,
            store: store.clone(),
            connections: connections.clone(),
            community: community.clone(),
            local_member,
            community_id,
            pending_registrations,
            voice_presence: voice_presence.clone(),
            voice_presence_commands: voice_presence_commands.clone(),
            voice_presence_receives,
            events: events.clone(),
            metrics: metrics.clone(),
        };
        let router = Router::builder(endpoint)
            .accept(ALPN, handler.clone())
            .spawn();
        let (reconnect_shutdown, shutdown) = oneshot::channel();
        let reconnect_task = tokio::spawn(reconnect(
            router.endpoint().clone(),
            handler.clone(),
            shutdown,
        ));
        Ok(Self {
            router,
            handler,
            signing_key,
            community_id,
            store,
            connections,
            community,
            metrics,
            voice_presence,
            voice_presence_commands,
            events,
            reconnect_shutdown: Some(reconnect_shutdown),
            reconnect_task: Some(reconnect_task),
        })
    }

    pub fn address(&self) -> PeerAddress {
        PeerAddress(self.router.endpoint().addr())
    }

    pub async fn community_invite(&self) -> Result<CommunityInvite, NodeError> {
        if self.community.read().await.owner() != self.member_id() {
            return Err(NodeError::authorization(
                "only the community owner can create an invite",
            ));
        }
        self.handler.egregore.ensure_recovery_reconciled().await?;
        Ok(CommunityInvite {
            community_id: self.community_id,
            owner_address: self.address(),
        })
    }

    pub fn member_id(&self) -> MemberId {
        MemberId::from_bytes(self.signing_key.verifying_key().to_bytes())
    }

    pub const fn community_id(&self) -> CommunityId {
        self.community_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn connection_diagnostics(&self) -> Vec<PeerDiagnostic> {
        self.connections
            .lock()
            .await
            .iter()
            .map(|peer| {
                let paths = peer.connection.paths();
                PeerDiagnostic {
                    member: peer.member,
                    paths: paths
                        .iter()
                        .map(|path| ConnectionPathDiagnostic {
                            kind: if path.is_ip() {
                                ConnectionPathKind::Direct
                            } else if path.is_relay() {
                                ConnectionPathKind::Relay
                            } else {
                                ConnectionPathKind::Custom
                            },
                            selected: path.is_selected(),
                            rtt: path.rtt(),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    pub async fn metrics_snapshot(&self) -> MetricsSnapshot {
        let mut snapshot = self.metrics.snapshot();

        if let Ok(counts) = self.store.debug_counts().await {
            snapshot.messages_total = counts.messages;
            snapshot.attachments_total = counts.attachments;
            snapshot.channels_total = counts.channels;
        }
        snapshot.db_bytes = tokio::fs::metadata(self.store.db_path())
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        snapshot.members_total = self.community.read().await.member_count() as u64;
        if let Ok(Some(epoch)) = self.store.latest_content_epoch().await {
            snapshot.content_epoch = epoch.number;
        }
        if let Ok(revision) = self.store.latest_membership_revision().await {
            snapshot.membership_revision = revision;
        }

        let endpoint = self.router.endpoint().metrics();
        snapshot.recv_datagrams = endpoint.socket.recv_datagrams.get();
        snapshot.send_relay = endpoint.socket.send_relay.get();
        snapshot.recv_data_relay = endpoint.socket.recv_data_relay.get();
        snapshot.holepunch_attempts = endpoint.socket.holepunch_attempts.get();
        snapshot.conns_opened = endpoint.socket.num_conns_opened.get();
        snapshot.conns_closed = endpoint.socket.num_conns_closed.get();

        snapshot
    }

    pub async fn export_identity(
        &self,
        path: impl AsRef<Path>,
        passphrase: &str,
    ) -> Result<(), NodeError> {
        crate::identity::export(path, passphrase, self.store.identity_material().await?)
    }

    pub async fn connect(&self, address: PeerAddress) -> Result<(), NodeError> {
        connect_peer(self.router.endpoint(), &self.handler, address.0).await
    }

    pub async fn execute(&self, command: Command) -> Result<(), NodeError> {
        match command {
            Command::SetDisplayName(name) => self.set_display_name(name).await,
            Command::CreateChannel(channel) => self.create_channel(channel).await,
            Command::PostText(message) => self.post_text(message).await,
            Command::ShareAttachment(attachment) => self.share_attachment(attachment).await,
            Command::ForgetAttachment { author, id } => {
                self.store.forget_attachment(author, id).await?;
                let _ = self.events.send(Event::AttachmentForgotten { author, id });
                Ok(())
            }
            Command::SendVoice(frame) => self.send_voice(frame).await,
            Command::SetVoicePresence { channel, state } => {
                self.set_voice_presence(channel, state).await
            }
            Command::ChangeMembership(change) => self.change_membership(change).await,
        }
    }

    pub async fn snapshot(&self) -> Result<Snapshot, NodeError> {
        self.store.snapshot().await
    }

    pub async fn last_read(&self, channel: ChannelId) -> Result<Option<MessageId>, NodeError> {
        require_channel(&self.store, channel, ChannelKind::Text).await?;
        self.store.last_read(channel).await
    }

    pub async fn set_last_read(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), NodeError> {
        require_channel(&self.store, channel, ChannelKind::Text).await?;
        self.store.set_last_read(channel, message).await
    }

    pub async fn shutdown(mut self) -> Result<(), NodeError> {
        if let Some(shutdown) = self.reconnect_shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.reconnect_task.take() {
            let _ = task.await;
        }
        self.router.shutdown().await.map_err(NodeError::network)
    }

    async fn post_text(&self, message: TextMessage) -> Result<(), NodeError> {
        let outcome = self.handler.egregore.post_text(message).await?;
        self.handler.finish_apply(outcome).await;
        self.metrics.messages_sent.inc();
        Ok(())
    }

    async fn set_display_name(&self, name: DisplayName) -> Result<(), NodeError> {
        let outcome = self.handler.egregore.set_display_name(name).await?;
        self.handler.finish_apply(outcome).await;
        Ok(())
    }

    async fn share_attachment(&self, attachment: crate::Attachment) -> Result<(), NodeError> {
        let outcome = self.handler.egregore.share_attachment(attachment).await?;
        self.handler.finish_apply(outcome).await;
        self.metrics.messages_sent.inc();
        Ok(())
    }

    async fn create_channel(&self, channel: Channel) -> Result<(), NodeError> {
        let outcome = self.handler.egregore.create_channel(channel).await?;
        self.handler.finish_apply(outcome).await;
        Ok(())
    }

    async fn change_membership(&self, change: MembershipChange) -> Result<(), NodeError> {
        let outcome = self.handler.egregore.change_membership(change).await?;
        self.handler.finish_apply(outcome).await;
        Ok(())
    }

    async fn broadcast_content_operation(
        &self,
        operation: wire::Operation,
    ) -> Result<(), NodeError> {
        let transition_subject = match operation.body.as_ref() {
            Some(wire::operation::Body::Membership(operation)) => operation
                .member
                .as_slice()
                .try_into()
                .ok()
                .map(MemberId::from_bytes),
            _ => None,
        };
        let bytes = operation.encode_to_vec();
        let community = self.community.read().await;
        let mut peers = self.connections.lock().await.clone();
        peers.sort_by_key(|peer| Some(peer.member) != transition_subject);
        for peer in peers {
            if community.authorize_participant(peer.member).is_err() {
                continue;
            }
            if let Err(error) = send_operation(&peer, &bytes).await {
                let _ = self.events.send(Event::Fault(error));
            }
        }
        Ok(())
    }

    async fn send_voice(&self, frame: VoiceFrame) -> Result<(), NodeError> {
        let community = self.community.read().await;
        community.authorize_participant(self.member_id())?;
        require_channel(&self.store, frame.channel_id(), ChannelKind::Voice).await?;
        if self
            .voice_presence
            .lock()
            .await
            .get(&self.member_id())
            .is_none_or(|presence| presence.channel != frame.channel_id())
        {
            return Err(NodeError::authorization(
                "voice transmission requires active channel presence",
            ));
        }
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        let (nonce, ciphertext) =
            encrypt_voice(self.community_id, epoch, self.member_id(), &frame)?;
        let bytes = wire::VoiceFrame {
            stream_id: frame.stream_id().as_bytes().to_vec(),
            sequence: frame.sequence(),
            payload: ciphertext,
            channel_id: frame.channel_id().as_bytes().to_vec(),
            epoch: epoch.number,
            membership_head: epoch.head.to_vec(),
            nonce: nonce.to_vec(),
        }
        .encode_to_vec();
        for peer in self.connections.lock().await.clone() {
            if community.authorize_participant(peer.member).is_err() {
                continue;
            }
            if let Err(error) = peer.connection.send_datagram(bytes.clone().into()) {
                let _ = self.events.send(Event::Fault(NodeError::network(error)));
            }
        }
        self.metrics.voice_frames_sent.inc();
        Ok(())
    }

    async fn set_voice_presence(
        &self,
        channel: ChannelId,
        state: VoicePresence,
    ) -> Result<(), NodeError> {
        let _guard = self.voice_presence_commands.lock().await;
        let member = self.member_id();
        self.community.read().await.authorize_participant(member)?;
        require_channel(&self.store, channel, ChannelKind::Voice).await?;
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        let encrypted =
            EncryptedVoicePresence::encrypt(self.community_id, epoch, member, channel, state)?;
        let events = {
            let mut active = self.voice_presence.lock().await;
            apply_voice_presence(&mut active, member, channel, state)?
        };
        send_voice_presence_events(&self.events, events);
        self.broadcast_content_operation(voice_presence_operation(&encrypted))
            .await
    }
}

#[derive(Debug, Clone)]
struct PeerConnection {
    member: MemberId,
    connection: Connection,
    origin: ConnectionOrigin,
}

impl PeerConnection {
    fn new(connection: Connection, origin: ConnectionOrigin) -> Self {
        let member = MemberId::from_bytes(*connection.remote_id().as_bytes());
        Self {
            member,
            connection,
            origin,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionOrigin {
    Incoming,
    Outgoing,
}

struct InitialSyncGuard {
    peer: Option<PeerConnection>,
    handler: OperationHandler,
}

impl InitialSyncGuard {
    fn new(peer: PeerConnection, handler: OperationHandler) -> Self {
        InitialSyncGuard {
            peer: Some(peer),
            handler,
        }
    }

    fn complete(&mut self) {
        self.peer = None;
    }
}

impl Drop for InitialSyncGuard {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.take() {
            peer.connection
                .close(0u32.into(), b"initial sync cancelled");
            let handler = self.handler.clone();
            tokio::spawn(async move { handler.untrack_connection(&peer).await });
        }
    }
}

fn replace_connection(
    local: MemberId,
    remote: MemberId,
    existing: ConnectionOrigin,
    new: ConnectionOrigin,
) -> bool {
    existing != new
        && new
            == if local < remote {
                ConnectionOrigin::Outgoing
            } else {
                ConnectionOrigin::Incoming
            }
}

fn reconnect_candidates(
    community: &Community,
    local: MemberId,
    connected: &[MemberId],
) -> Vec<MemberId> {
    if !community.contains(local) {
        return Vec::new();
    }
    community
        .members()
        .filter(|member| local < *member && !connected.contains(member))
        .collect()
}

fn reconnect_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs((1_u64 << attempt.min(5)).min(30))
}

async fn reconnect_attempt(
    shutdown: &mut oneshot::Receiver<()>,
    timeout: std::time::Duration,
    attempt: impl std::future::Future<Output = Result<(), NodeError>>,
) -> Option<bool> {
    tokio::select! {
        _ = shutdown => None,
        result = tokio::time::timeout(timeout, attempt) => {
            Some(matches!(result, Ok(Ok(()))))
        }
    }
}

async fn connect_peer(
    endpoint: &Endpoint,
    handler: &OperationHandler,
    address: EndpointAddr,
) -> Result<(), NodeError> {
    let connection = endpoint
        .connect(address, ALPN)
        .await
        .map_err(NodeError::network)?;
    let connection = PeerConnection::new(connection, ConnectionOrigin::Outgoing);
    if !handler.track_connection(connection.clone()).await {
        return Ok(());
    }
    let _ = handler.events.send(Event::PeerConnected(connection.member));
    let receiver = connection.clone();
    let receiver_handler = handler.clone();
    tokio::spawn(async move { receiver_handler.receive(receiver).await });
    let mut guard = InitialSyncGuard::new(connection.clone(), handler.clone());
    if let Err(error) = handler.sync_peer(&connection).await {
        connection
            .connection
            .close(0u32.into(), b"initial sync failed");
        handler.untrack_connection(&connection).await;
        guard.complete();
        if handler.connections.lock().await.iter().any(|peer| {
            peer.member == connection.member && peer.connection.close_reason().is_none()
        }) {
            return Ok(());
        }
        return Err(error);
    }
    guard.complete();
    Ok(())
}

async fn reconnect(
    endpoint: Endpoint,
    handler: OperationHandler,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut retries = BTreeMap::<MemberId, (u32, tokio::time::Instant)>::new();
    loop {
        let connected = handler
            .connections
            .lock()
            .await
            .iter()
            .filter(|peer| peer.connection.close_reason().is_none())
            .map(|peer| peer.member)
            .collect::<Vec<_>>();
        let candidates = reconnect_candidates(
            &*handler.community.read().await,
            handler.local_member,
            &connected,
        );
        retries.retain(|member, _| candidates.contains(member));

        for member in candidates {
            if handler
                .connections
                .lock()
                .await
                .iter()
                .any(|peer| peer.member == member && peer.connection.close_reason().is_none())
            {
                retries.remove(&member);
                continue;
            }
            let now = tokio::time::Instant::now();
            let (attempt, retry_at) = retries.entry(member).or_insert((0, now));
            if *retry_at > now {
                continue;
            }
            let endpoint_id = match iroh::EndpointId::from_bytes(member.as_bytes()) {
                Ok(endpoint_id) => endpoint_id,
                Err(_) => {
                    *retry_at = now + reconnect_delay(*attempt);
                    *attempt = attempt.saturating_add(1);
                    continue;
                }
            };
            let Some(connected) = reconnect_attempt(
                &mut shutdown,
                RECONNECT_ATTEMPT_TIMEOUT,
                connect_peer(&endpoint, &handler, endpoint_id.into()),
            )
            .await
            else {
                return;
            };
            if connected {
                retries.remove(&member);
            } else {
                *retry_at = tokio::time::Instant::now() + reconnect_delay(*attempt);
                *attempt = attempt.saturating_add(1);
            }
        }

        // ponytail: polling keeps membership and disconnect wakeups on one path; use Notify if
        // idle wakeups become measurable.
        tokio::select! {
            _ = &mut shutdown => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
    }
}

#[derive(Debug, Clone)]
struct OperationHandler {
    egregore: Arc<Egregore>,
    store: Arc<Store>,
    connections: Arc<Mutex<Vec<PeerConnection>>>,
    community: Arc<RwLock<Community>>,
    local_member: MemberId,
    community_id: CommunityId,
    pending_registrations: Arc<Mutex<BTreeMap<MemberId, KeyRegistration>>>,
    voice_presence: Arc<Mutex<BTreeMap<MemberId, ActiveVoicePresence>>>,
    voice_presence_commands: Arc<Mutex<()>>,
    voice_presence_receives: Arc<Mutex<()>>,
    events: broadcast::Sender<Event>,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
struct Egregore {
    store: Arc<Store>,
    community: Arc<RwLock<Community>>,
    local_member: MemberId,
    community_id: CommunityId,
    hpke_seed: [u8; 32],
    signing_key: SigningKey,
    pending_registrations: Arc<Mutex<BTreeMap<MemberId, KeyRegistration>>>,
    membership_commands: Arc<Mutex<()>>,
    profile_commands: Arc<Mutex<()>>,
    metrics: Arc<Metrics>,
}

#[derive(Default)]
struct ApplyOutcome {
    events: Vec<Event>,
    outbound: Vec<wire::Operation>,
    removed_member: Option<MemberId>,
    sync_member: Option<MemberId>,
    refresh_voice_presence: bool,
}

impl ProtocolHandler for OperationHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let connection = PeerConnection::new(connection, ConnectionOrigin::Incoming);
        if !self.track_connection(connection.clone()).await {
            return Ok(());
        }
        let _ = self.events.send(Event::PeerConnected(connection.member));
        let handler = self.clone();
        let sync_peer = connection.clone();
        tokio::spawn(async move {
            if let Err(error) = handler.sync_peer(&sync_peer).await {
                sync_peer
                    .connection
                    .close(0u32.into(), b"initial sync failed");
                handler.untrack_connection(&sync_peer).await;
                if handler.connections.lock().await.iter().any(|peer| {
                    peer.member == sync_peer.member && peer.connection.close_reason().is_none()
                }) {
                    return;
                }
                let _ = handler.events.send(Event::Fault(error));
            }
        });
        self.receive(connection).await;
        Ok(())
    }
}

impl OperationHandler {
    async fn track_connection(&self, peer: PeerConnection) -> bool {
        let mut connections = self.connections.lock().await;
        connections.retain(|existing| existing.connection.close_reason().is_none());
        if let Some(index) = connections
            .iter()
            .position(|existing| existing.member == peer.member)
        {
            if !replace_connection(
                self.local_member,
                peer.member,
                connections[index].origin,
                peer.origin,
            ) {
                peer.connection.close(0u32.into(), b"duplicate connection");
                return false;
            }
            let loser = connections.remove(index);
            loser.connection.close(0u32.into(), b"duplicate connection");
        }
        connections.push(peer);
        true
    }

    async fn untrack_connection(&self, peer: &PeerConnection) {
        let id = peer.connection.stable_id();
        let mut connections = self.connections.lock().await;
        connections.retain(|existing| existing.connection.stable_id() != id);
        let still_connected = connections
            .iter()
            .any(|existing| existing.member == peer.member);
        drop(connections);
        if !still_connected && !self.community.read().await.contains(peer.member) {
            self.pending_registrations.lock().await.remove(&peer.member);
        }
        if !still_connected {
            clear_voice_presence(&self.voice_presence, &self.events, peer.member).await;
        }
    }

    async fn sync_peer(&self, peer: &PeerConnection) -> Result<(), NodeError> {
        let local_registration = self
            .store
            .key_registration(self.local_member)
            .await?
            .ok_or_else(|| NodeError::protocol("local encryption key is not registered"))?;
        send_operation(
            peer,
            &registration_operation(&local_registration).encode_to_vec(),
        )
        .await?;
        if self
            .community
            .read()
            .await
            .authorize_exchange(self.local_member, peer.member)
            .is_err()
        {
            return Ok(());
        }

        let (memberships, registrations, envelopes, channels, messages, attachments, profiles) =
            self.store.sync_state().await?;
        for registration in registrations {
            if registration.member() != self.local_member {
                send_operation(peer, &registration_operation(&registration).encode_to_vec())
                    .await?;
            }
        }
        for envelope in envelopes {
            send_operation(peer, &envelope_operation(&envelope).encode_to_vec()).await?;
        }
        for membership in memberships {
            send_operation(peer, &membership_operation(&membership).encode_to_vec()).await?;
        }
        for channel in channels {
            send_operation(peer, &channel_operation(&channel).encode_to_vec()).await?;
        }
        let remote_inventory = exchange_text_inventory(peer, &text_inventory(&messages)).await?;
        let messages = encrypted_messages_missing(messages, &remote_inventory);
        for message in messages {
            if let Err(error) =
                send_operation(peer, &text_operation(&message).encode_to_vec()).await
            {
                let _ = self.events.send(Event::Fault(error));
            }
        }
        for attachment in attachments {
            if let Err(error) =
                send_operation(peer, &attachment_operation(&attachment).encode_to_vec()).await
            {
                let _ = self.events.send(Event::Fault(error));
            }
        }
        for profile in profiles {
            if let Err(error) =
                send_operation(peer, &member_profile_operation(&profile).encode_to_vec()).await
            {
                let _ = self.events.send(Event::Fault(error));
            }
        }
        self.sync_local_voice_presence(peer).await?;
        Ok(())
    }

    async fn sync_local_voice_presence(&self, peer: &PeerConnection) -> Result<(), NodeError> {
        let _guard = self.voice_presence_commands.lock().await;
        if self
            .community
            .read()
            .await
            .authorize_participant(peer.member)
            .is_err()
        {
            return Ok(());
        }
        let Some(presence) = self
            .voice_presence
            .lock()
            .await
            .get(&self.local_member)
            .copied()
        else {
            return Ok(());
        };
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        for state in [VoicePresence::Joined, VoicePresence::Muted(presence.muted)] {
            let encrypted = EncryptedVoicePresence::encrypt(
                self.community_id,
                epoch,
                self.local_member,
                presence.channel,
                state,
            )?;
            let bytes = voice_presence_operation(&encrypted).encode_to_vec();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                send_operation(peer, &bytes),
            )
            .await
            .map_err(|_| {
                NodeError::network(format!(
                    "voice presence sync to {} via {:?} timed out",
                    short_member(peer.member),
                    peer.origin
                ))
            })??;
        }
        Ok(())
    }

    async fn receive(&self, peer: PeerConnection) {
        loop {
            let result = tokio::select! {
                stream = peer.connection.accept_bi() => match stream {
                    Ok((send, recv)) => self.receive_operation(peer.member, send, recv).await,
                    Err(_) => break,
                },
                datagram = peer.connection.read_datagram() => match datagram {
                    Ok(bytes) => self.receive_voice(peer.member, &bytes).await,
                    Err(_) => break,
                },
            };
            if let Err(error) = result {
                tracing::warn!(%error, "rejected peer operation");
                let _ = self.events.send(Event::Fault(error));
            }
        }
        self.untrack_connection(&peer).await;
    }

    async fn receive_operation(
        &self,
        peer: MemberId,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), NodeError> {
        let bytes = recv
            .read_to_end(MAX_OPERATION_BYTES)
            .await
            .map_err(NodeError::network)?;
        let operation = wire::Operation::decode(bytes.as_slice())?;
        let body = operation
            .body
            .ok_or_else(|| NodeError::protocol("operation has no body"))?;
        if let wire::operation::Body::TextInventory(inventory) = &body {
            self.community
                .read()
                .await
                .authorize_exchange(peer, self.local_member)?;
            decode_text_inventory(inventory.clone())?;
            let response = encode_text_inventory(&self.store.encrypted_text_inventory().await?);
            send.write_all(&response)
                .await
                .map_err(NodeError::network)?;
            send.finish().map_err(NodeError::network)?;
            return Ok(());
        }
        let outcome = match body {
            wire::operation::Body::Text(operation) => {
                self.egregore.receive_text(peer, operation).await?
            }
            wire::operation::Body::Membership(operation) => {
                self.egregore.receive_membership(operation).await?
            }
            wire::operation::Body::CreateChannel(operation) => {
                self.egregore.receive_channel(operation).await?
            }
            wire::operation::Body::KeyRegistration(operation) => {
                self.egregore.receive_registration(peer, operation).await?
            }
            wire::operation::Body::ContentKeyEnvelope(operation) => {
                self.egregore.receive_envelope(operation).await?
            }
            wire::operation::Body::Attachment(operation) => {
                self.egregore.receive_attachment(peer, operation).await?
            }
            wire::operation::Body::MemberProfile(operation) => {
                self.egregore
                    .receive_member_profile(peer, operation)
                    .await?
            }
            wire::operation::Body::VoicePresence(operation) => {
                self.receive_voice_presence(peer, operation).await?;
                ApplyOutcome::default()
            }
            wire::operation::Body::TextInventory(_) => unreachable!(),
        };
        self.finish_apply(outcome).await;
        send.write_all(&[1]).await.map_err(NodeError::network)?;
        send.finish().map_err(NodeError::network)?;
        Ok(())
    }

    async fn finish_apply(&self, outcome: ApplyOutcome) {
        if let Some(member) = outcome.removed_member {
            let _commands = self.voice_presence_commands.lock().await;
            let _guard = self.voice_presence_receives.lock().await;
            clear_voice_presence(&self.voice_presence, &self.events, member).await;
        }
        for event in outcome.events {
            let _ = self.events.send(event);
        }
        if let Some(member) = outcome.sync_member
            && let Some(peer) = self
                .connections
                .lock()
                .await
                .iter()
                .find(|peer| peer.member == member)
                .cloned()
            && let Err(error) = self.sync_peer(&peer).await
        {
            let _ = self.events.send(Event::Fault(error));
        }
        for operation in outcome.outbound {
            let transition_subject = match operation.body.as_ref() {
                Some(wire::operation::Body::Membership(operation)) => operation
                    .member
                    .as_slice()
                    .try_into()
                    .ok()
                    .map(MemberId::from_bytes),
                _ => None,
            };
            let bytes = operation.encode_to_vec();
            let community = self.community.read().await.clone();
            let mut peers = self.connections.lock().await.clone();
            peers.sort_by_key(|peer| Some(peer.member) != transition_subject);
            for peer in peers {
                let is_departing_member = outcome.removed_member == Some(peer.member)
                    && transition_subject == Some(peer.member);
                if !community.contains(peer.member) && !is_departing_member {
                    continue;
                }
                if let Err(error) = send_operation(&peer, &bytes).await {
                    let _ = self.events.send(Event::Fault(error));
                }
                if is_departing_member {
                    peer.connection.close(0u32.into(), b"membership removed");
                    self.untrack_connection(&peer).await;
                }
            }
        }
        if outcome.refresh_voice_presence {
            for peer in self.connections.lock().await.clone() {
                if self.community.read().await.role(peer.member) == Some(MemberRole::Participant)
                    && let Err(error) = self.sync_local_voice_presence(&peer).await
                {
                    let _ = self.events.send(Event::Fault(error));
                }
            }
        }
    }
}

impl Egregore {
    async fn ensure_recovery_reconciled(&self) -> Result<(), NodeError> {
        if self.store.is_recovered().await? && self.store.active_content_epoch().await?.is_none() {
            return Err(NodeError::authorization(
                "recovered identity must synchronize before administering the community",
            ));
        }
        Ok(())
    }

    async fn post_text(&self, message: TextMessage) -> Result<ApplyOutcome, NodeError> {
        self.community
            .read()
            .await
            .authorize_participant(self.local_member)?;
        require_channel(&self.store, message.channel_id(), ChannelKind::Text).await?;
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        let encrypted =
            EncryptedText::encrypt(&self.signing_key, self.community_id, epoch, message)?;
        let authored = encrypted.decrypt(self.community_id, &epoch.key)?;
        let events = if self.store.insert_encrypted(&encrypted).await? {
            vec![Event::TextStored(authored)]
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            outbound: vec![text_operation(&encrypted)],
            ..ApplyOutcome::default()
        })
    }

    async fn set_display_name(&self, name: DisplayName) -> Result<ApplyOutcome, NodeError> {
        self.community
            .read()
            .await
            .authorize_participant(self.local_member)?;
        let _guard = self.profile_commands.lock().await;
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        let revision = self
            .store
            .next_member_profile_revision(self.local_member)
            .await?;
        let profile = EncryptedMemberProfile::encrypt(
            &self.signing_key,
            self.community_id,
            epoch,
            revision,
            name.clone(),
        )?;
        let events = if self.store.insert_member_profile(&profile).await? {
            vec![Event::DisplayNameChanged {
                member: self.local_member,
                name,
            }]
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            outbound: vec![member_profile_operation(&profile)],
            ..ApplyOutcome::default()
        })
    }

    async fn share_attachment(
        &self,
        attachment: crate::Attachment,
    ) -> Result<ApplyOutcome, NodeError> {
        self.community
            .read()
            .await
            .authorize_participant(self.local_member)?;
        require_channel(&self.store, attachment.channel_id(), ChannelKind::Text).await?;
        let epoch = self
            .store
            .active_content_epoch()
            .await?
            .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
        let encrypted =
            EncryptedAttachment::encrypt(&self.signing_key, self.community_id, epoch, attachment)?;
        let authored = encrypted.decrypt(self.community_id, &epoch.key)?;
        let events = if self.store.insert_encrypted_attachment(&encrypted).await? {
            vec![Event::AttachmentStored(authored)]
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            outbound: vec![attachment_operation(&encrypted)],
            ..ApplyOutcome::default()
        })
    }

    async fn create_channel(&self, channel: Channel) -> Result<ApplyOutcome, NodeError> {
        if self.community.read().await.owner() != self.local_member {
            return Err(NodeError::authorization(
                "only the community owner can create channels",
            ));
        }
        let signed = SignedCreateChannel::sign(&self.signing_key, self.community_id, channel);
        let events = if self.store.insert_channel(&signed).await? {
            vec![Event::ChannelCreated(signed.channel().clone())]
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            outbound: vec![channel_operation(&signed)],
            ..ApplyOutcome::default()
        })
    }

    async fn change_membership(&self, change: MembershipChange) -> Result<ApplyOutcome, NodeError> {
        let _guard = self.membership_commands.lock().await;
        self.ensure_recovery_reconciled().await?;
        let mut next = self.community.read().await.clone();
        if !next.change_membership(self.local_member, change)? {
            return Ok(ApplyOutcome::default());
        }
        let revision = self.store.next_membership_revision().await?;
        if change.is_admission() {
            // ponytail: the registration normally arrives on the accept-side sync task;
            // replace this bounded rendezvous with a protocol handshake if setup latency matters.
            for _ in 0..100 {
                if self
                    .store
                    .key_registration(change.member())
                    .await?
                    .is_some()
                {
                    break;
                }
                if let Some(registration) = self
                    .pending_registrations
                    .lock()
                    .await
                    .remove(&change.member())
                {
                    self.store.insert_key_registration(&registration).await?;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if self
                .store
                .key_registration(change.member())
                .await?
                .is_none()
            {
                return Err(NodeError::authorization(
                    "member has not registered an encryption key",
                ));
            }
        }
        let signed = SignedMembership::sign(&self.signing_key, self.community_id, revision, change);
        let epoch = ContentEpoch {
            number: revision,
            head: signed.head(self.community_id),
            key: rand::random(),
        };
        let mut registrations = Vec::new();
        let mut envelopes = Vec::new();
        for member in next.members() {
            let registration = self.store.key_registration(member).await?.ok_or_else(|| {
                NodeError::authorization("community member has no encryption key registration")
            })?;
            if next.role(member) == Some(MemberRole::Participant) {
                envelopes.push(ContentKeyEnvelope::seal(
                    &self.signing_key,
                    self.community_id,
                    epoch,
                    &registration,
                )?);
            }
            registrations.push(registration);
        }
        self.store
            .persist_rotation(&signed, epoch, &envelopes)
            .await?;
        let community = self.store.community().await?;
        *self.community.write().await = community.clone();

        let refreshed_profile = {
            let _guard = self.profile_commands.lock().await;
            refresh_local_profile(
                &self.store,
                self.community_id,
                self.local_member,
                &self.signing_key,
            )
            .await
        };
        let events = vec![Event::MembershipChanged(community)];
        let mut outbound = registrations
            .iter()
            .map(registration_operation)
            .chain(envelopes.iter().map(envelope_operation))
            .collect::<Vec<_>>();
        outbound.push(membership_operation(&signed));
        let mut outcome = ApplyOutcome {
            events,
            outbound,
            removed_member: (!change.is_admission()).then_some(change.member()),
            sync_member: change.is_admission().then_some(change.member()),
            refresh_voice_presence: false,
        };
        finish_profile_refresh(&mut outcome, refreshed_profile);
        Ok(outcome)
    }

    async fn receive_text(
        &self,
        peer: MemberId,
        operation: wire::TextMessage,
    ) -> Result<ApplyOutcome, NodeError> {
        let id = operation
            .id
            .try_into()
            .map_err(|_| NodeError::protocol("message id is not 32 bytes"))?;
        let author = operation
            .author
            .try_into()
            .map_err(|_| NodeError::protocol("message author id is not 32 bytes"))?;
        let signature = operation
            .signature
            .try_into()
            .map_err(|_| NodeError::protocol("message signature is not 64 bytes"))?;
        let channel_id = operation
            .channel_id
            .try_into()
            .map_err(|_| NodeError::protocol("message channel id is not 32 bytes"))?;
        if !operation.body.is_empty() {
            return Err(NodeError::protocol(
                "live plaintext text operations are unsupported",
            ));
        }
        let head = operation
            .membership_head
            .try_into()
            .map_err(|_| NodeError::protocol("membership head is not 32 bytes"))?;
        let nonce = operation
            .nonce
            .try_into()
            .map_err(|_| NodeError::protocol("text nonce is not 24 bytes"))?;
        let encrypted = EncryptedText {
            id: MessageId::from_bytes(id),
            channel_id: ChannelId::from_bytes(channel_id),
            author: MemberId::from_bytes(author),
            epoch: operation.epoch,
            head,
            nonce,
            ciphertext: operation.ciphertext,
            signature,
        };
        encrypted.verify(self.community_id)?;
        self.store
            .authorize_epoch_author(encrypted.epoch, &encrypted.head, encrypted.author)
            .await?;
        self.community
            .read()
            .await
            .authorize_exchange(peer, self.local_member)?;
        require_channel(&self.store, encrypted.channel_id, ChannelKind::Text).await?;
        let key = self
            .local_content_key(encrypted.epoch, &encrypted.head)
            .await?;
        let authored = key
            .map(|key| {
                encrypted
                    .decrypt(self.community_id, &key)
                    .inspect_err(|_| self.metrics.decrypt_failures.inc())
            })
            .transpose()?;
        let events = if self.store.insert_encrypted(&encrypted).await? {
            if authored.is_some() {
                self.metrics.messages_received.inc();
            }
            authored.into_iter().map(Event::TextStored).collect()
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            ..ApplyOutcome::default()
        })
    }

    async fn receive_attachment(
        &self,
        peer: MemberId,
        operation: wire::Attachment,
    ) -> Result<ApplyOutcome, NodeError> {
        let encrypted = EncryptedAttachment {
            id: MessageId::from_bytes(
                operation
                    .id
                    .try_into()
                    .map_err(|_| NodeError::protocol("attachment id is not 32 bytes"))?,
            ),
            author: MemberId::from_bytes(
                operation
                    .author
                    .try_into()
                    .map_err(|_| NodeError::protocol("attachment author is not 32 bytes"))?,
            ),
            channel_id: ChannelId::from_bytes(
                operation
                    .channel_id
                    .try_into()
                    .map_err(|_| NodeError::protocol("attachment channel is not 32 bytes"))?,
            ),
            epoch: operation.epoch,
            head: operation
                .membership_head
                .try_into()
                .map_err(|_| NodeError::protocol("attachment membership head is not 32 bytes"))?,
            nonce: operation
                .nonce
                .try_into()
                .map_err(|_| NodeError::protocol("attachment nonce is not 24 bytes"))?,
            ciphertext: operation.ciphertext,
            signature: operation
                .signature
                .try_into()
                .map_err(|_| NodeError::protocol("attachment signature is not 64 bytes"))?,
        };
        if encrypted.ciphertext.len() > MAX_ATTACHMENT_BYTES + MAX_ATTACHMENT_NAME_BYTES + 18 {
            return Err(NodeError::protocol(
                "attachment ciphertext exceeds the size limit",
            ));
        }
        encrypted.verify(self.community_id)?;
        self.store
            .authorize_epoch_author(encrypted.epoch, &encrypted.head, encrypted.author)
            .await?;
        self.community
            .read()
            .await
            .authorize_exchange(peer, self.local_member)?;
        require_channel(&self.store, encrypted.channel_id, ChannelKind::Text).await?;
        let authored = self
            .local_content_key(encrypted.epoch, &encrypted.head)
            .await?
            .map(|key| {
                encrypted
                    .decrypt(self.community_id, &key)
                    .inspect_err(|_| self.metrics.decrypt_failures.inc())
            })
            .transpose()?;
        let events = if self.store.insert_encrypted_attachment(&encrypted).await? {
            if authored.is_some() {
                self.metrics.messages_received.inc();
            }
            authored.into_iter().map(Event::AttachmentStored).collect()
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            ..ApplyOutcome::default()
        })
    }

    async fn receive_member_profile(
        &self,
        peer: MemberId,
        operation: wire::MemberProfile,
    ) -> Result<ApplyOutcome, NodeError> {
        let profile = EncryptedMemberProfile {
            member: MemberId::from_bytes(
                operation
                    .member
                    .try_into()
                    .map_err(|_| NodeError::protocol("member profile member is not 32 bytes"))?,
            ),
            revision: operation.revision,
            epoch: operation.epoch,
            head: operation.membership_head.try_into().map_err(|_| {
                NodeError::protocol("member profile membership head is not 32 bytes")
            })?,
            nonce: operation
                .nonce
                .try_into()
                .map_err(|_| NodeError::protocol("member profile nonce is not 24 bytes"))?,
            ciphertext: operation.ciphertext,
            signature: operation
                .signature
                .try_into()
                .map_err(|_| NodeError::protocol("member profile signature is not 64 bytes"))?,
        };
        if profile.revision == 0 {
            return Err(NodeError::protocol("member profile revision is zero"));
        }
        if profile.ciphertext.len() > crate::model::MAX_DISPLAY_NAME_BYTES + 16 {
            return Err(NodeError::protocol(
                "member profile ciphertext exceeds the size limit",
            ));
        }
        profile.verify(self.community_id)?;
        self.store
            .authorize_epoch_author(profile.epoch, &profile.head, profile.member)
            .await?;
        let community = self.community.read().await;
        community.authorize_exchange(peer, self.local_member)?;
        community.authorize_participant(profile.member)?;
        drop(community);
        let name = self
            .local_content_key(profile.epoch, &profile.head)
            .await?
            .map(|key| {
                profile
                    .decrypt(self.community_id, &key)
                    .inspect_err(|_| self.metrics.decrypt_failures.inc())
            })
            .transpose()?;
        let events = if self.store.insert_member_profile(&profile).await? {
            name.map(|name| Event::DisplayNameChanged {
                member: profile.member,
                name,
            })
            .into_iter()
            .collect()
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            ..ApplyOutcome::default()
        })
    }

    async fn local_content_key(
        &self,
        epoch: u64,
        head: &[u8; 32],
    ) -> Result<Option<[u8; 32]>, NodeError> {
        let key = self.store.content_key(epoch, head).await?;
        if key.is_none()
            && self.community.read().await.role(self.local_member) == Some(MemberRole::Participant)
        {
            return Err(NodeError::protocol(
                "participant content key is unavailable",
            ));
        }
        Ok(key)
    }
}

impl OperationHandler {
    async fn receive_voice_presence(
        &self,
        peer: MemberId,
        operation: wire::VoicePresence,
    ) -> Result<(), NodeError> {
        let _guard = self.voice_presence_receives.lock().await;
        self.community.read().await.authorize_participant(peer)?;
        let encrypted = EncryptedVoicePresence {
            epoch: operation.epoch,
            head: operation
                .membership_head
                .try_into()
                .map_err(|_| NodeError::protocol("voice presence head is not 32 bytes"))?,
            nonce: operation
                .nonce
                .try_into()
                .map_err(|_| NodeError::protocol("voice presence nonce is not 24 bytes"))?,
            ciphertext: operation.ciphertext,
        };
        if encrypted.ciphertext.len() > MAX_VOICE_PRESENCE_CIPHERTEXT {
            return Err(NodeError::protocol(
                "voice presence ciphertext exceeds the size limit",
            ));
        }
        require_active_epoch(&self.store, encrypted.epoch, &encrypted.head).await?;
        let key = self
            .store
            .content_key(encrypted.epoch, &encrypted.head)
            .await?
            .ok_or_else(|| NodeError::protocol("voice presence content key is unavailable"))?;
        let (channel, state) = encrypted
            .decrypt(self.community_id, peer, &key)
            .inspect_err(|_| self.metrics.decrypt_failures.inc())?;
        require_channel(&self.store, channel, ChannelKind::Voice).await?;
        let events = {
            let mut active = self.voice_presence.lock().await;
            apply_voice_presence(&mut active, peer, channel, state)?
        };
        send_voice_presence_events(&self.events, events);
        Ok(())
    }
}

impl Egregore {
    async fn receive_registration(
        &self,
        peer: MemberId,
        operation: wire::KeyRegistration,
    ) -> Result<ApplyOutcome, NodeError> {
        let member = MemberId::from_bytes(
            operation
                .member
                .try_into()
                .map_err(|_| NodeError::protocol("key registration member is not 32 bytes"))?,
        );
        let public_key = operation
            .public_key
            .try_into()
            .map_err(|_| NodeError::protocol("HPKE public key is not 32 bytes"))?;
        let signature = operation
            .signature
            .try_into()
            .map_err(|_| NodeError::protocol("key registration signature is not 64 bytes"))?;
        let registration =
            KeyRegistration::verified(self.community_id, member, public_key, signature)?;
        if self.store.key_registration(member).await?.is_some() {
            self.store.insert_key_registration(&registration).await?;
            return Ok(ApplyOutcome::default());
        }
        let community = self.community.read().await;
        let peer_is_admitted = community.contains(peer);
        if community.contains(member) || peer == community.owner() {
            drop(community);
            self.store.insert_key_registration(&registration).await?;
        } else if member == peer || peer_is_admitted {
            drop(community);
            let mut pending = self.pending_registrations.lock().await;
            if !pending.contains_key(&member) && pending.len() >= MAX_PENDING_REGISTRATIONS {
                return Err(NodeError::authorization(
                    "too many pending member registrations",
                ));
            }
            pending.insert(member, registration);
        } else {
            return Err(NodeError::authorization(
                "pending member key registration came from an unauthorized peer",
            ));
        }
        Ok(ApplyOutcome::default())
    }

    async fn receive_envelope(
        &self,
        operation: wire::ContentKeyEnvelope,
    ) -> Result<ApplyOutcome, NodeError> {
        let envelope = ContentKeyEnvelope {
            epoch: operation.epoch,
            head: operation
                .membership_head
                .try_into()
                .map_err(|_| NodeError::protocol("membership head is not 32 bytes"))?,
            recipient: MemberId::from_bytes(
                operation
                    .recipient
                    .try_into()
                    .map_err(|_| NodeError::protocol("envelope recipient is not 32 bytes"))?,
            ),
            encapsulated_key: operation
                .encapsulated_key
                .try_into()
                .map_err(|_| NodeError::protocol("encapsulated key is not 32 bytes"))?,
            ciphertext: operation.ciphertext,
            signature: operation
                .signature
                .try_into()
                .map_err(|_| NodeError::protocol("envelope signature is not 64 bytes"))?,
        };
        envelope.verify(self.store.community_owner().await?, self.community_id)?;
        if envelope.recipient == self.local_member
            && self.community.read().await.role(self.local_member) == Some(MemberRole::Availability)
        {
            return Err(NodeError::authorization(
                "availability peer cannot receive a content key envelope",
            ));
        }
        self.store.insert_content_key_envelope(&envelope).await?;
        if envelope.recipient == self.local_member {
            self.store
                .insert_content_key(envelope.open(self.community_id, &self.hpke_seed)?)
                .await?;
        }
        Ok(ApplyOutcome::default())
    }

    async fn receive_channel(
        &self,
        operation: wire::CreateChannel,
    ) -> Result<ApplyOutcome, NodeError> {
        let id = operation
            .id
            .try_into()
            .map_err(|_| NodeError::protocol("channel id is not 32 bytes"))?;
        let kind = ChannelKind::from_number(i64::from(operation.kind))
            .ok_or_else(|| NodeError::protocol("channel kind is invalid"))?;
        let signature = operation
            .signature
            .try_into()
            .map_err(|_| NodeError::protocol("channel signature is not 64 bytes"))?;
        let channel = Channel::new(ChannelId::from_bytes(id), operation.name, kind)?;
        let owner = self.store.community_owner().await?;
        let signed = SignedCreateChannel::verified(owner, self.community_id, channel, signature)
            .map_err(NodeError::protocol)?;
        let events = if self.store.insert_channel(&signed).await? {
            vec![Event::ChannelCreated(signed.channel().clone())]
        } else {
            Vec::new()
        };
        Ok(ApplyOutcome {
            events,
            ..ApplyOutcome::default()
        })
    }

    async fn receive_membership(
        &self,
        operation: wire::MembershipChange,
    ) -> Result<ApplyOutcome, NodeError> {
        let member = operation
            .member
            .try_into()
            .map_err(|_| NodeError::protocol("membership member id is not 32 bytes"))?;
        let signature = operation
            .signature
            .try_into()
            .map_err(|_| NodeError::protocol("membership signature is not 64 bytes"))?;
        if operation.availability && !operation.admitted {
            return Err(NodeError::protocol(
                "removed membership cannot have an availability role",
            ));
        }
        let change = if operation.availability {
            MembershipChange::AdmitAvailability(MemberId::from_bytes(member))
        } else if operation.admitted {
            MembershipChange::Admit(MemberId::from_bytes(member))
        } else {
            MembershipChange::Remove(MemberId::from_bytes(member))
        };
        if matches!(change, MembershipChange::AdmitAvailability(member) if member == self.local_member)
            && self.store.has_content_keys().await?
        {
            return Err(NodeError::authorization(
                "an identity with content keys cannot become an availability peer",
            ));
        }
        let update = MembershipUpdate::new(operation.revision, change);
        let owner = self.store.community_owner().await?;
        let signed = SignedMembership::verified(owner, self.community_id, update, signature)
            .map_err(NodeError::protocol)?;
        let expected_revision = self.store.next_membership_revision().await?;
        if operation.revision < expected_revision {
            self.store.insert_membership(&signed).await?;
            return Ok(ApplyOutcome::default());
        }
        if operation.revision != expected_revision {
            return Err(NodeError::protocol("membership revision has a gap"));
        }
        let mut next = self.community.read().await.clone();
        if !next.change_membership(owner, change)? {
            self.store.insert_membership(&signed).await?;
            return Ok(ApplyOutcome::default());
        }
        let head = signed.head(self.community_id);
        let local_was_removed =
            change.member() == self.local_member && !next.contains(self.local_member);
        if !local_was_removed {
            for member in next.members() {
                if self.store.key_registration(member).await?.is_none()
                    && let Some(registration) =
                        self.pending_registrations.lock().await.remove(&member)
                {
                    self.store.insert_key_registration(&registration).await?;
                }
                if self.store.key_registration(member).await?.is_none() {
                    return Err(NodeError::protocol(
                        "membership activates a member without an encryption key registration",
                    ));
                }
                let envelope = self
                    .store
                    .content_key_envelope(operation.revision, &head, member)
                    .await?;
                match next.role(member) {
                    Some(MemberRole::Participant) => envelope
                        .ok_or_else(|| {
                            NodeError::protocol("membership arrived before its recipient envelopes")
                        })?
                        .verify(owner, self.community_id)?,
                    Some(MemberRole::Availability) if envelope.is_some() => {
                        return Err(NodeError::protocol(
                            "availability peer received a content key envelope",
                        ));
                    }
                    _ => {}
                }
            }
        }
        if !next.contains(change.member())
            && self
                .store
                .content_key_envelope(operation.revision, &head, change.member())
                .await?
                .is_some()
        {
            return Err(NodeError::protocol(
                "removed member received the replacement content key",
            ));
        }
        if self.store.insert_membership(&signed).await? {
            let community = self.store.community().await?;
            *self.community.write().await = community.clone();
            let mut outcome = ApplyOutcome {
                events: vec![Event::MembershipChanged(community)],
                removed_member: (!change.is_admission()).then_some(change.member()),
                ..ApplyOutcome::default()
            };
            let local_is_participant = self.community.read().await.role(self.local_member)
                == Some(MemberRole::Participant);
            if local_is_participant {
                let refreshed_profile = {
                    let _guard = self.profile_commands.lock().await;
                    refresh_local_profile(
                        &self.store,
                        self.community_id,
                        self.local_member,
                        &self.signing_key,
                    )
                    .await
                };
                finish_profile_refresh(&mut outcome, refreshed_profile);
            }
            outcome.refresh_voice_presence = local_is_participant;
            return Ok(outcome);
        }
        Ok(ApplyOutcome::default())
    }
}

impl OperationHandler {
    async fn receive_voice(&self, author: MemberId, bytes: &[u8]) -> Result<(), NodeError> {
        let community = self.community.read().await;
        community.authorize_participant(author)?;
        community.authorize_participant(self.local_member)?;
        let operation = wire::VoiceFrame::decode(bytes)?;
        let stream_id = operation
            .stream_id
            .try_into()
            .map_err(|_| NodeError::protocol("voice stream id is not 16 bytes"))?;
        let channel_id = operation
            .channel_id
            .try_into()
            .map_err(|_| NodeError::protocol("voice channel id is not 32 bytes"))?;
        let head: [u8; 32] = operation
            .membership_head
            .try_into()
            .map_err(|_| NodeError::protocol("voice membership head is not 32 bytes"))?;
        let nonce: [u8; 24] = operation
            .nonce
            .try_into()
            .map_err(|_| NodeError::protocol("voice nonce is not 24 bytes"))?;
        require_active_epoch(&self.store, operation.epoch, &head).await?;
        let encrypted = VoiceFrame::in_channel(
            VoiceStreamId::from_bytes(stream_id),
            ChannelId::from_bytes(channel_id),
            operation.sequence,
            operation.payload,
        )?;
        require_channel(&self.store, encrypted.channel_id(), ChannelKind::Voice).await?;
        if self
            .voice_presence
            .lock()
            .await
            .get(&author)
            .is_none_or(|presence| presence.channel != encrypted.channel_id())
        {
            return Err(NodeError::authorization(
                "voice author is not present in the channel",
            ));
        }
        let key = self
            .store
            .content_key(operation.epoch, &head)
            .await?
            .ok_or_else(|| NodeError::protocol("voice content key is unavailable"))?;
        let epoch = ContentEpoch {
            number: operation.epoch,
            head,
            key,
        };
        let payload = decrypt_voice(self.community_id, epoch, author, &encrypted, &nonce)
            .inspect_err(|_| self.metrics.voice_frame_failures.inc())?;
        self.metrics.voice_frames_received.inc();
        let frame = VoiceFrame::in_channel(
            encrypted.stream_id(),
            encrypted.channel_id(),
            encrypted.sequence(),
            payload,
        )?;
        let _ = self
            .events
            .send(Event::VoiceReceived(AuthoredVoice::new(author, frame)));
        Ok(())
    }
}

fn text_operation(signed: &EncryptedText) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::Text(wire::TextMessage {
            id: signed.id.as_bytes().to_vec(),
            body: String::new(),
            author: signed.author.as_bytes().to_vec(),
            signature: signed.signature.to_vec(),
            channel_id: signed.channel_id.as_bytes().to_vec(),
            epoch: signed.epoch,
            membership_head: signed.head.to_vec(),
            nonce: signed.nonce.to_vec(),
            ciphertext: signed.ciphertext.clone(),
        })),
    }
}

fn voice_presence_operation(presence: &EncryptedVoicePresence) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::VoicePresence(wire::VoicePresence {
            epoch: presence.epoch,
            membership_head: presence.head.to_vec(),
            nonce: presence.nonce.to_vec(),
            ciphertext: presence.ciphertext.clone(),
        })),
    }
}

fn apply_voice_presence(
    active: &mut BTreeMap<MemberId, ActiveVoicePresence>,
    member: MemberId,
    channel: ChannelId,
    state: VoicePresence,
) -> Result<Vec<Event>, NodeError> {
    let current = active.get(&member).copied();
    let mut events = Vec::new();
    match state {
        VoicePresence::Joined => {
            if current.is_some_and(|current| current.channel == channel) {
                return Ok(events);
            }
            let channel_members = active
                .iter()
                .filter_map(|(member, presence)| (presence.channel == channel).then_some(*member))
                .collect::<Vec<_>>();
            if channel_members.len() >= MAX_VOICE_PARTICIPANTS {
                // ponytail: deterministic MemberId ordering resolves concurrent fourth joins;
                // replace with owner-issued room leases only if adversarial fairness matters.
                let displaced = channel_members
                    .into_iter()
                    .max()
                    .expect("a full channel has a participant");
                if member > displaced {
                    return Err(NodeError::authorization("voice channel is full"));
                }
                let displaced_presence = active
                    .remove(&displaced)
                    .expect("the displaced participant was selected from active presence");
                events.push(Event::VoicePresence {
                    channel: displaced_presence.channel,
                    member: displaced,
                    state: VoicePresence::Left,
                });
            }
            if let Some(current) = current {
                active.remove(&member);
                events.push(Event::VoicePresence {
                    channel: current.channel,
                    member,
                    state: VoicePresence::Left,
                });
            }
            active.insert(
                member,
                ActiveVoicePresence {
                    channel,
                    muted: false,
                },
            );
            events.push(Event::VoicePresence {
                channel,
                member,
                state: VoicePresence::Joined,
            });
        }
        VoicePresence::Left => {
            if current.is_some_and(|current| current.channel == channel) {
                active.remove(&member);
                events.push(Event::VoicePresence {
                    channel,
                    member,
                    state: VoicePresence::Left,
                });
            }
        }
        VoicePresence::Muted(muted) => {
            let current = active.get_mut(&member).ok_or_else(|| {
                NodeError::protocol("voice mute requires an active voice presence")
            })?;
            if current.channel != channel {
                return Err(NodeError::protocol(
                    "voice mute references a different channel",
                ));
            }
            if current.muted != muted {
                current.muted = muted;
                events.push(Event::VoicePresence {
                    channel,
                    member,
                    state: VoicePresence::Muted(muted),
                });
            }
        }
    }
    Ok(events)
}

async fn clear_voice_presence(
    active: &Mutex<BTreeMap<MemberId, ActiveVoicePresence>>,
    events: &broadcast::Sender<Event>,
    member: MemberId,
) {
    if let Some(presence) = active.lock().await.remove(&member) {
        let _ = events.send(Event::VoicePresence {
            channel: presence.channel,
            member,
            state: VoicePresence::Left,
        });
    }
}

fn send_voice_presence_events(sender: &broadcast::Sender<Event>, events: Vec<Event>) {
    for event in events {
        let _ = sender.send(event);
    }
}

fn short_member(member: MemberId) -> String {
    member
        .as_bytes()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn attachment_operation(attachment: &EncryptedAttachment) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::Attachment(wire::Attachment {
            id: attachment.id.as_bytes().to_vec(),
            author: attachment.author.as_bytes().to_vec(),
            channel_id: attachment.channel_id.as_bytes().to_vec(),
            epoch: attachment.epoch,
            membership_head: attachment.head.to_vec(),
            nonce: attachment.nonce.to_vec(),
            ciphertext: attachment.ciphertext.clone(),
            signature: attachment.signature.to_vec(),
        })),
    }
}

fn member_profile_operation(profile: &EncryptedMemberProfile) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::MemberProfile(wire::MemberProfile {
            member: profile.member.as_bytes().to_vec(),
            revision: profile.revision,
            epoch: profile.epoch,
            membership_head: profile.head.to_vec(),
            nonce: profile.nonce.to_vec(),
            ciphertext: profile.ciphertext.clone(),
            signature: profile.signature.to_vec(),
        })),
    }
}

fn text_inventory(messages: &[EncryptedText]) -> BTreeSet<(MemberId, MessageId)> {
    messages
        .iter()
        .map(|message| (message.author, message.id))
        .collect()
}

fn text_inventory_operation(inventory: &BTreeSet<(MemberId, MessageId)>) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::TextInventory(wire::TextInventory {
            messages: inventory
                .iter()
                .map(|(author, id)| wire::EncryptedTextIdentity {
                    author: author.as_bytes().to_vec(),
                    id: id.as_bytes().to_vec(),
                })
                .collect(),
        })),
    }
}

fn encode_text_inventory(inventory: &BTreeSet<(MemberId, MessageId)>) -> Vec<u8> {
    let bytes = text_inventory_operation(inventory).encode_to_vec();
    if bytes.len() <= MAX_OPERATION_BYTES {
        bytes
    } else {
        // ponytail: full replay preserves convergence until inventory paging is warranted.
        text_inventory_operation(&BTreeSet::new()).encode_to_vec()
    }
}

fn encrypted_messages_missing(
    messages: Vec<EncryptedText>,
    remote_inventory: &BTreeSet<(MemberId, MessageId)>,
) -> Vec<EncryptedText> {
    messages
        .into_iter()
        .filter(|message| !remote_inventory.contains(&(message.author, message.id)))
        .collect()
}

fn decode_text_inventory(
    inventory: wire::TextInventory,
) -> Result<BTreeSet<(MemberId, MessageId)>, NodeError> {
    inventory
        .messages
        .into_iter()
        .map(|message| {
            let author = message
                .author
                .try_into()
                .map(MemberId::from_bytes)
                .map_err(|_| NodeError::protocol("text inventory author is not 32 bytes"))?;
            let id = message
                .id
                .try_into()
                .map(MessageId::from_bytes)
                .map_err(|_| NodeError::protocol("text inventory message id is not 32 bytes"))?;
            Ok((author, id))
        })
        .collect()
}

async fn exchange_text_inventory(
    peer: &PeerConnection,
    inventory: &BTreeSet<(MemberId, MessageId)>,
) -> Result<BTreeSet<(MemberId, MessageId)>, NodeError> {
    let bytes = encode_text_inventory(inventory);
    let (mut send, mut recv) = peer
        .connection
        .open_bi()
        .await
        .map_err(NodeError::network)?;
    send.write_all(&bytes).await.map_err(NodeError::network)?;
    send.finish().map_err(NodeError::network)?;
    let response = recv
        .read_to_end(MAX_OPERATION_BYTES)
        .await
        .map_err(NodeError::network)?;
    let operation = wire::Operation::decode(response.as_slice())?;
    let Some(wire::operation::Body::TextInventory(inventory)) = operation.body else {
        return Err(NodeError::protocol(
            "peer did not respond with a text inventory",
        ));
    };
    decode_text_inventory(inventory)
}

fn registration_operation(registration: &KeyRegistration) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::KeyRegistration(
            wire::KeyRegistration {
                member: registration.member().as_bytes().to_vec(),
                public_key: registration.public_key().to_vec(),
                signature: registration.signature().to_vec(),
            },
        )),
    }
}

fn envelope_operation(envelope: &ContentKeyEnvelope) -> wire::Operation {
    wire::Operation {
        body: Some(wire::operation::Body::ContentKeyEnvelope(
            wire::ContentKeyEnvelope {
                epoch: envelope.epoch,
                membership_head: envelope.head.to_vec(),
                recipient: envelope.recipient.as_bytes().to_vec(),
                encapsulated_key: envelope.encapsulated_key.to_vec(),
                ciphertext: envelope.ciphertext.clone(),
                signature: envelope.signature.to_vec(),
            },
        )),
    }
}

fn channel_operation(signed: &SignedCreateChannel) -> wire::Operation {
    let channel = signed.channel();
    wire::Operation {
        body: Some(wire::operation::Body::CreateChannel(wire::CreateChannel {
            id: channel.id().as_bytes().to_vec(),
            name: channel.name().to_owned(),
            kind: channel.kind().number(),
            signature: signed.signature().to_vec(),
        })),
    }
}

fn membership_operation(signed: &SignedMembership) -> wire::Operation {
    let update = signed.update();
    wire::Operation {
        body: Some(wire::operation::Body::Membership(wire::MembershipChange {
            revision: update.revision(),
            member: update.change().member().as_bytes().to_vec(),
            admitted: update.change().is_admission(),
            availability: update.change().is_availability(),
            signature: signed.signature().to_vec(),
        })),
    }
}

async fn refresh_local_profile(
    store: &Store,
    community_id: CommunityId,
    member: MemberId,
    signing_key: &SigningKey,
) -> Result<Option<(EncryptedMemberProfile, DisplayName)>, NodeError> {
    let Some(current) = store.member_profile(member).await? else {
        return Ok(None);
    };
    let epoch = store
        .active_content_epoch()
        .await?
        .ok_or_else(|| NodeError::authorization("content key is unavailable"))?;
    if current.epoch == epoch.number && current.head == epoch.head {
        return Ok(None);
    }
    let old_key = store
        .content_key(current.epoch, &current.head)
        .await?
        .ok_or_else(|| NodeError::protocol("member profile content key is unavailable"))?;
    let name = current.decrypt(community_id, &old_key)?;
    let profile = EncryptedMemberProfile::encrypt(
        signing_key,
        community_id,
        epoch,
        current
            .revision
            .checked_add(1)
            .ok_or_else(|| NodeError::protocol("member profile revision overflow"))?,
        name.clone(),
    )?;
    store.insert_member_profile(&profile).await?;
    Ok(Some((profile, name)))
}

fn finish_profile_refresh(
    outcome: &mut ApplyOutcome,
    result: Result<Option<(EncryptedMemberProfile, DisplayName)>, NodeError>,
) {
    match result {
        Ok(Some((profile, name))) => {
            outcome.events.push(Event::DisplayNameChanged {
                member: profile.member,
                name,
            });
            outcome.outbound.push(member_profile_operation(&profile));
        }
        Ok(None) => {}
        Err(error) => outcome.events.push(Event::Fault(error)),
    }
}

async fn send_operation(peer: &PeerConnection, bytes: &[u8]) -> Result<(), NodeError> {
    if bytes.len() > MAX_OPERATION_BYTES {
        return Err(NodeError::protocol("operation exceeds transport limit"));
    }
    let (mut send, mut recv) = peer
        .connection
        .open_bi()
        .await
        .map_err(NodeError::network)?;
    send.write_all(bytes).await.map_err(NodeError::network)?;
    send.finish().map_err(NodeError::network)?;
    let acknowledgement = recv.read_to_end(1).await.map_err(NodeError::network)?;
    if acknowledgement != [1] {
        return Err(NodeError::protocol("peer did not acknowledge persistence"));
    }
    Ok(())
}

async fn require_channel(
    store: &Store,
    id: ChannelId,
    expected: ChannelKind,
) -> Result<(), NodeError> {
    let channel = store
        .channel(id)
        .await?
        .ok_or_else(|| NodeError::authorization("channel does not exist"))?;
    if channel.kind() != expected {
        return Err(NodeError::authorization(
            "channel kind does not match operation",
        ));
    }
    Ok(())
}

async fn require_active_epoch(store: &Store, epoch: u64, head: &[u8; 32]) -> Result<(), NodeError> {
    if store.active_membership_head().await? != (epoch, *head) {
        return Err(NodeError::authorization(
            "content is not encrypted for the active membership head",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(byte: u8) -> MemberId {
        MemberId::from_bytes([byte; 32])
    }

    #[test]
    fn profile_refresh_failure_keeps_membership_applied() {
        let mut outcome = ApplyOutcome {
            events: vec![Event::MembershipChanged(Community::new(member(1)))],
            ..ApplyOutcome::default()
        };

        finish_profile_refresh(
            &mut outcome,
            Err(NodeError::protocol("profile refresh failed")),
        );

        assert!(matches!(outcome.events[0], Event::MembershipChanged(_)));
        assert!(matches!(outcome.events[1], Event::Fault(_)));
    }

    #[test]
    fn reconnect_candidates_are_admitted_remote_dial_owners_only() {
        let local = member(2);
        let mut community = Community::new(member(1));
        community
            .change_membership(member(1), MembershipChange::Admit(local))
            .unwrap();
        community
            .change_membership(member(1), MembershipChange::Admit(member(3)))
            .unwrap();
        community
            .change_membership(member(1), MembershipChange::Admit(member(4)))
            .unwrap();

        assert_eq!(
            reconnect_candidates(&community, local, &[member(3)]),
            vec![member(4)]
        );
    }

    #[test]
    fn complete_text_inventory_selects_no_payloads() {
        let author = member(1);
        let id = MessageId::from_bytes([2; 32]);
        let message = EncryptedText {
            id,
            channel_id: ChannelId::GENERAL,
            author,
            epoch: 1,
            head: [3; 32],
            nonce: [4; 24],
            ciphertext: vec![5],
            signature: [6; 64],
        };

        assert!(
            encrypted_messages_missing(vec![message], &BTreeSet::from([(author, id)])).is_empty()
        );
    }

    #[test]
    fn oversized_text_inventory_falls_back_to_full_replay() {
        let author = member(1);
        let inventory = (0..150_000_u64)
            .map(|index| {
                let mut id = [0; 32];
                id[..8].copy_from_slice(&index.to_be_bytes());
                (author, MessageId::from_bytes(id))
            })
            .collect();

        let operation = wire::Operation::decode(encode_text_inventory(&inventory).as_slice())
            .expect("fallback inventory decodes");
        let Some(wire::operation::Body::TextInventory(inventory)) = operation.body else {
            panic!("fallback is a text inventory");
        };
        assert!(inventory.messages.is_empty());
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(reconnect_delay(0), std::time::Duration::from_secs(1));
        assert_eq!(reconnect_delay(3), std::time::Duration::from_secs(8));
        assert_eq!(reconnect_delay(99), std::time::Duration::from_secs(30));
    }

    #[test]
    fn duplicate_connection_prefers_the_owned_direction() {
        assert!(replace_connection(
            member(1),
            member(2),
            ConnectionOrigin::Incoming,
            ConnectionOrigin::Outgoing,
        ));
        assert!(!replace_connection(
            member(2),
            member(1),
            ConnectionOrigin::Incoming,
            ConnectionOrigin::Outgoing,
        ));
        assert!(!replace_connection(
            member(1),
            member(2),
            ConnectionOrigin::Outgoing,
            ConnectionOrigin::Outgoing,
        ));
    }

    #[tokio::test]
    async fn timed_out_reconnect_attempt_does_not_block_the_next() {
        let (_shutdown, mut shutdown) = oneshot::channel();
        assert_eq!(
            reconnect_attempt(
                &mut shutdown,
                std::time::Duration::from_millis(1),
                std::future::pending(),
            )
            .await,
            Some(false)
        );
        assert_eq!(
            reconnect_attempt(
                &mut shutdown,
                std::time::Duration::from_secs(1),
                std::future::ready(Ok(())),
            )
            .await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn duplicate_connection_closes_the_losing_transport() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
        let remote = Endpoint::builder(presets::Minimal).bind().await.unwrap();
        let first = remote.connect(node.address().0, ALPN).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while node.connections.lock().await.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let duplicate = remote.connect(node.address().0, ALPN).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), duplicate.closed())
            .await
            .expect("duplicate transport should be closed");
        assert!(first.close_reason().is_none());
        assert_eq!(node.connections.lock().await.len(), 1);

        node.shutdown().await.unwrap();
        remote.close().await;
    }

    #[tokio::test]
    async fn cancelled_initial_sync_closes_and_untracks() {
        let directory = tempfile::tempdir().unwrap();
        let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
        let remote = Endpoint::builder(presets::Minimal).bind().await.unwrap();
        let connection = remote.connect(node.address().0, ALPN).await.unwrap();
        let tracked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(peer) = node.connections.lock().await.first().cloned() {
                    break peer;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let stable_id = tracked.connection.stable_id();

        drop(InitialSyncGuard::new(tracked, node.handler.clone()));

        tokio::time::timeout(std::time::Duration::from_secs(5), connection.closed())
            .await
            .expect("cancelled sync transport should be closed");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while node
                .connections
                .lock()
                .await
                .iter()
                .any(|peer| peer.connection.stable_id() == stable_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled sync should be untracked");

        node.shutdown().await.unwrap();
        remote.close().await;
    }

    #[tokio::test]
    async fn existing_mode_rejects_an_uninitialized_database() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("peer.db"), []).unwrap();

        let error = match Node::open(NodeConfig::new(directory.path()).existing()).await {
            Ok(_) => panic!("uninitialized database should be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), FaultKind::Protocol);
        assert!(error.to_string().contains("community is not initialized"));
    }

    #[test]
    fn voice_presence_enforces_cap_and_one_channel_per_member() {
        let channel = ChannelId::from_bytes([7; 32]);
        let other = ChannelId::from_bytes([8; 32]);
        let mut active = BTreeMap::new();
        for byte in 1..=4 {
            assert_eq!(
                apply_voice_presence(&mut active, member(byte), channel, VoicePresence::Joined)
                    .unwrap()
                    .len(),
                1
            );
        }
        assert_eq!(
            apply_voice_presence(&mut active, member(5), channel, VoicePresence::Joined)
                .unwrap_err()
                .kind(),
            FaultKind::Authorization
        );

        let moved =
            apply_voice_presence(&mut active, member(1), other, VoicePresence::Joined).unwrap();
        assert!(matches!(
            moved.as_slice(),
            [
                Event::VoicePresence {
                    channel: left,
                    state: VoicePresence::Left,
                    ..
                },
                Event::VoicePresence {
                    channel: joined,
                    state: VoicePresence::Joined,
                    ..
                }
            ] if *left == channel && *joined == other
        ));
    }

    #[test]
    fn concurrent_full_room_joins_choose_the_same_participants() {
        let channel = ChannelId::from_bytes([9; 32]);
        let mut active = BTreeMap::new();
        for byte in 2..=5 {
            apply_voice_presence(&mut active, member(byte), channel, VoicePresence::Joined)
                .unwrap();
        }

        let events =
            apply_voice_presence(&mut active, member(1), channel, VoicePresence::Joined).unwrap();

        assert!(!active.contains_key(&member(5)));
        assert!(active.contains_key(&member(1)));
        assert!(matches!(
            events.as_slice(),
            [
                Event::VoicePresence {
                    member: displaced,
                    state: VoicePresence::Left,
                    ..
                },
                Event::VoicePresence {
                    member: joined,
                    state: VoicePresence::Joined,
                    ..
                }
            ] if *displaced == member(5) && *joined == member(1)
        ));
    }
}
