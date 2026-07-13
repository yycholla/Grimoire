use std::collections::BTreeMap;

use crate::{Community, CommunityId, MemberId, MemberRole, MembershipChange};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use thiserror::Error;

pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_VOICE_FRAME_BYTES: usize = 1024;
pub const MAX_VOICE_PARTICIPANTS: usize = 4;
pub const MAX_CHANNEL_NAME_BYTES: usize = 64;
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 255;
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayName(String);

impl DisplayName {
    pub fn new(value: impl Into<String>) -> Result<Self, DisplayNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(DisplayNameError::Empty);
        }
        if value.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(DisplayNameError::TooLong);
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(DisplayNameError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DisplayNameError {
    #[error("display name is empty")]
    Empty,
    #[error("display name exceeds 64 bytes")]
    TooLong,
    #[error("display name has surrounding whitespace or control characters")]
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChannelId([u8; 32]);

impl ChannelId {
    pub const GENERAL: Self = Self([0; 32]);
    pub const VOICE_ROOM: Self = Self([1; 32]);

    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelKind {
    Text,
    Voice,
}

impl ChannelKind {
    pub(crate) const fn number(self) -> u32 {
        match self {
            Self::Text => 0,
            Self::Voice => 1,
        }
    }

    pub(crate) const fn from_number(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Text),
            1 => Some(Self::Voice),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Channel {
    id: ChannelId,
    name: String,
    kind: ChannelKind,
}

impl Channel {
    pub fn new(
        id: ChannelId,
        name: impl Into<String>,
        kind: ChannelKind,
    ) -> Result<Self, ChannelError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ChannelError::EmptyName);
        }
        if name.len() > MAX_CHANNEL_NAME_BYTES {
            return Err(ChannelError::NameTooLong);
        }
        if name.trim() != name || name.chars().any(char::is_control) {
            return Err(ChannelError::InvalidName);
        }
        Ok(Self { id, name, kind })
    }

    pub const fn id(&self) -> ChannelId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn kind(&self) -> ChannelKind {
        self.kind
    }

    pub(crate) fn default_channels() -> [Self; 2] {
        [
            Self::new(ChannelId::GENERAL, "general", ChannelKind::Text)
                .expect("default text channel is valid"),
            Self::new(ChannelId::VOICE_ROOM, "Voice Room", ChannelKind::Voice)
                .expect("default voice channel is valid"),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ChannelError {
    #[error("channel name is empty")]
    EmptyName,
    #[error("channel name exceeds 64 bytes")]
    NameTooLong,
    #[error("channel name has surrounding whitespace or control characters")]
    InvalidName,
}

#[derive(Clone, Debug)]
pub(crate) struct SignedCreateChannel {
    channel: Channel,
    signature: [u8; 64],
}

impl SignedCreateChannel {
    pub(crate) fn sign(
        signing_key: &SigningKey,
        community_id: CommunityId,
        channel: Channel,
    ) -> Self {
        let signature = signing_key
            .sign(&channel_signing_bytes(community_id, &channel))
            .to_bytes();
        Self { channel, signature }
    }

    pub(crate) fn verified(
        owner: MemberId,
        community_id: CommunityId,
        channel: Channel,
        signature: [u8; 64],
    ) -> Result<Self, ed25519_dalek::SignatureError> {
        let key = VerifyingKey::from_bytes(owner.as_bytes())?;
        let signature = Signature::from_bytes(&signature);
        let current = key.verify_strict(&channel_signing_bytes(community_id, &channel), &signature);
        if current.is_err() && community_id == CommunityId::legacy(owner) {
            key.verify_strict(&legacy_channel_signing_bytes(&channel), &signature)?;
        } else {
            current?;
        }
        Ok(Self {
            channel,
            signature: signature.to_bytes(),
        })
    }

    pub(crate) const fn channel(&self) -> &Channel {
        &self.channel
    }

    pub(crate) const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }
}

fn channel_signing_bytes(community_id: CommunityId, channel: &Channel) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(channel.name().len() + 96);
    bytes.extend_from_slice(b"peer-community/channel/v2\0");
    bytes.extend_from_slice(community_id.as_bytes());
    append_channel_bytes(&mut bytes, channel);
    bytes
}

fn legacy_channel_signing_bytes(channel: &Channel) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(channel.name().len() + 64);
    bytes.extend_from_slice(b"peer-community/channel/v1\0");
    append_channel_bytes(&mut bytes, channel);
    bytes
}

fn append_channel_bytes(bytes: &mut Vec<u8>, channel: &Channel) {
    bytes.extend_from_slice(channel.id().as_bytes());
    bytes.push(channel.kind().number() as u8);
    bytes.extend_from_slice(&(channel.name().len() as u64).to_be_bytes());
    bytes.extend_from_slice(channel.name().as_bytes());
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MessageId([u8; 32]);

impl MessageId {
    pub fn generate() -> Self {
        let mut bytes: [u8; 32] = rand::random();
        let milliseconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        bytes[..8].copy_from_slice(&milliseconds.to_be_bytes());
        Self(bytes)
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextMessage {
    id: MessageId,
    channel_id: ChannelId,
    body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoredText {
    author: MemberId,
    message: TextMessage,
}

impl AuthoredText {
    pub(crate) fn new(author: MemberId, message: TextMessage) -> Self {
        Self { author, message }
    }

    pub const fn author(&self) -> MemberId {
        self.author
    }

    pub const fn message(&self) -> &TextMessage {
        &self.message
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SignedText {
    authored: AuthoredText,
    signature: [u8; 64],
}

impl SignedText {
    fn new(authored: AuthoredText, signature: [u8; 64]) -> Self {
        Self {
            authored,
            signature,
        }
    }

    #[cfg(test)]
    pub(crate) fn sign(signing_key: &SigningKey, message: TextMessage) -> Self {
        let author = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        let signature = signing_key.sign(&text_signing_bytes(&message)).to_bytes();
        Self::new(AuthoredText::new(author, message), signature)
    }

    pub(crate) fn verified(
        authored: AuthoredText,
        signature: [u8; 64],
    ) -> Result<Self, ed25519_dalek::SignatureError> {
        let signed = Self::new(authored, signature);
        signed.verify()?;
        Ok(signed)
    }

    pub(crate) const fn authored(&self) -> &AuthoredText {
        &self.authored
    }

    #[cfg(test)]
    pub(crate) const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    fn verify(&self) -> Result<(), ed25519_dalek::SignatureError> {
        let verifying_key = VerifyingKey::from_bytes(self.authored.author().as_bytes())?;
        let signature = Signature::from_bytes(&self.signature);
        let current =
            verifying_key.verify_strict(&text_signing_bytes(self.authored.message()), &signature);
        if current.is_err() && self.authored.message().channel_id() == ChannelId::GENERAL {
            verifying_key.verify_strict(
                &legacy_text_signing_bytes(self.authored.message()),
                &signature,
            )
        } else {
            current
        }
    }
}

fn text_signing_bytes(message: &TextMessage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(message.body().len() + 64);
    bytes.extend_from_slice(b"peer-community/text/v2\0");
    bytes.extend_from_slice(message.id().as_bytes());
    bytes.extend_from_slice(message.channel_id().as_bytes());
    bytes.extend_from_slice(&(message.body().len() as u64).to_be_bytes());
    bytes.extend_from_slice(message.body().as_bytes());
    bytes
}

fn legacy_text_signing_bytes(message: &TextMessage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(message.body().len() + 64);
    bytes.extend_from_slice(b"peer-community/text/v1\0");
    bytes.extend_from_slice(message.id().as_bytes());
    bytes.extend_from_slice(&(message.body().len() as u64).to_be_bytes());
    bytes.extend_from_slice(message.body().as_bytes());
    bytes
}

impl TextMessage {
    pub fn new(id: MessageId, body: impl Into<String>) -> Result<Self, MessageError> {
        Self::in_channel(id, ChannelId::GENERAL, body)
    }

    pub fn in_channel(
        id: MessageId,
        channel_id: ChannelId,
        body: impl Into<String>,
    ) -> Result<Self, MessageError> {
        let body = body.into();
        if body.is_empty() {
            return Err(MessageError::Empty);
        }
        if body.len() > MAX_TEXT_BYTES {
            return Err(MessageError::TooLong);
        }
        Ok(Self {
            id,
            channel_id,
            body,
        })
    }

    pub const fn id(&self) -> MessageId {
        self.id
    }

    pub const fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MessageError {
    #[error("message body is empty")]
    Empty,
    #[error("message body exceeds 16 KiB")]
    TooLong,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    id: MessageId,
    channel_id: ChannelId,
    name: String,
    bytes: Vec<u8>,
}

impl Attachment {
    pub fn new(
        id: MessageId,
        channel_id: ChannelId,
        name: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, AttachmentError> {
        let name = name.into();
        Self::validate_name(&name)?;
        if bytes.is_empty() {
            return Err(AttachmentError::Empty);
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        Ok(Self {
            id,
            channel_id,
            name,
            bytes,
        })
    }

    pub(crate) fn validate_name(name: &str) -> Result<(), AttachmentError> {
        if name.is_empty()
            || name.len() > MAX_ATTACHMENT_NAME_BYTES
            || name.trim() != name
            || name.chars().any(char::is_control)
        {
            return Err(AttachmentError::InvalidName);
        }
        Ok(())
    }

    pub const fn id(&self) -> MessageId {
        self.id
    }

    pub const fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AttachmentError {
    #[error("attachment name is invalid")]
    InvalidName,
    #[error("attachment is empty")]
    Empty,
    #[error("attachment exceeds 8 MiB")]
    TooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoredAttachment {
    author: MemberId,
    attachment: Attachment,
}

impl AuthoredAttachment {
    pub(crate) fn new(author: MemberId, attachment: Attachment) -> Self {
        Self { author, attachment }
    }

    pub const fn author(&self) -> MemberId {
        self.author
    }

    pub const fn attachment(&self) -> &Attachment {
        &self.attachment
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VoiceStreamId([u8; 16]);

impl VoiceStreamId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceFrame {
    stream_id: VoiceStreamId,
    channel_id: ChannelId,
    sequence: u64,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoredVoice {
    author: MemberId,
    frame: VoiceFrame,
}

impl AuthoredVoice {
    pub(crate) fn new(author: MemberId, frame: VoiceFrame) -> Self {
        Self { author, frame }
    }

    pub const fn author(&self) -> MemberId {
        self.author
    }

    pub const fn frame(&self) -> &VoiceFrame {
        &self.frame
    }
}

impl VoiceFrame {
    pub fn new(
        stream_id: VoiceStreamId,
        sequence: u64,
        payload: Vec<u8>,
    ) -> Result<Self, VoiceFrameError> {
        Self::in_channel(stream_id, ChannelId::VOICE_ROOM, sequence, payload)
    }

    pub fn in_channel(
        stream_id: VoiceStreamId,
        channel_id: ChannelId,
        sequence: u64,
        payload: Vec<u8>,
    ) -> Result<Self, VoiceFrameError> {
        if payload.is_empty() {
            return Err(VoiceFrameError::Empty);
        }
        if payload.len() > MAX_VOICE_FRAME_BYTES {
            return Err(VoiceFrameError::TooLarge);
        }
        Ok(Self {
            stream_id,
            channel_id,
            sequence,
            payload,
        })
    }

    pub const fn stream_id(&self) -> VoiceStreamId {
        self.stream_id
    }

    pub const fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum VoiceFrameError {
    #[error("voice frame payload is empty")]
    Empty,
    #[error("voice frame payload exceeds 1 KiB")]
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoicePresence {
    Joined,
    Left,
    Muted(bool),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FaultKind {
    #[error("storage error")]
    Storage,
    #[error("filesystem error")]
    Filesystem,
    #[error("network error")]
    Network,
    #[error("protocol error")]
    Protocol,
    #[error("invalid message")]
    InvalidMessage,
    #[error("authorization error")]
    Authorization,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{kind}: {message}")]
pub struct Fault {
    kind: FaultKind,
    message: String,
}

impl Fault {
    pub const fn kind(&self) -> FaultKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn new(kind: FaultKind, error: impl std::fmt::Display) -> Self {
        Self {
            kind,
            message: error.to_string(),
        }
    }

    pub(crate) fn network(error: impl std::fmt::Display) -> Self {
        Self::new(FaultKind::Network, error)
    }

    pub(crate) fn protocol(error: impl std::fmt::Display) -> Self {
        Self::new(FaultKind::Protocol, error)
    }

    pub(crate) fn authorization(error: impl std::fmt::Display) -> Self {
        Self::new(FaultKind::Authorization, error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    SetDisplayName(DisplayName),
    CreateChannel(Channel),
    PostText(TextMessage),
    ShareAttachment(Attachment),
    ForgetAttachment {
        author: MemberId,
        id: MessageId,
    },
    SendVoice(VoiceFrame),
    SetVoicePresence {
        channel: ChannelId,
        state: VoicePresence,
    },
    ChangeMembership(MembershipChange),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    PeerConnected(MemberId),
    DisplayNameChanged {
        member: MemberId,
        name: DisplayName,
    },
    ChannelCreated(Channel),
    TextStored(AuthoredText),
    AttachmentStored(AuthoredAttachment),
    AttachmentForgotten {
        author: MemberId,
        id: MessageId,
    },
    VoiceReceived(AuthoredVoice),
    VoicePresence {
        channel: ChannelId,
        member: MemberId,
        state: VoicePresence,
    },
    MembershipChanged(Community),
    Fault(Fault),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    channels: Vec<Channel>,
    messages: Vec<AuthoredText>,
    attachments: Vec<AuthoredAttachment>,
    community: Community,
    members: Vec<MemberId>,
    display_names: BTreeMap<MemberId, DisplayName>,
}

impl Snapshot {
    pub(crate) fn new(
        channels: Vec<Channel>,
        messages: Vec<AuthoredText>,
        attachments: Vec<AuthoredAttachment>,
        community: Community,
        display_names: BTreeMap<MemberId, DisplayName>,
    ) -> Self {
        let members = community.members().collect();
        Self {
            channels,
            messages,
            attachments,
            community,
            members,
            display_names,
        }
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn messages(&self) -> &[AuthoredText] {
        &self.messages
    }

    pub fn attachments(&self) -> &[AuthoredAttachment] {
        &self.attachments
    }

    pub fn owner(&self) -> MemberId {
        self.community.owner()
    }

    pub fn members(&self) -> &[MemberId] {
        &self.members
    }

    pub fn role(&self, member: MemberId) -> Option<MemberRole> {
        self.community.role(member)
    }

    pub fn display_names(&self) -> &BTreeMap<MemberId, DisplayName> {
        &self.display_names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_signature_binds_the_channel() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let signed = SignedText::sign(
            &key,
            TextMessage::in_channel(MessageId::from_bytes([3; 32]), ChannelId::GENERAL, "hello")
                .unwrap(),
        );
        let moved = AuthoredText::new(
            signed.authored().author(),
            TextMessage::in_channel(
                MessageId::from_bytes([3; 32]),
                ChannelId::VOICE_ROOM,
                "hello",
            )
            .unwrap(),
        );
        assert!(SignedText::verified(moved, *signed.signature()).is_err());
    }

    #[test]
    fn legacy_text_signatures_migrate_only_to_general() {
        let key = SigningKey::from_bytes(&[8; 32]);
        let message = TextMessage::new(MessageId::from_bytes([4; 32]), "legacy").unwrap();
        let signature = key.sign(&legacy_text_signing_bytes(&message)).to_bytes();
        let author = MemberId::from_bytes(key.verifying_key().to_bytes());
        assert!(SignedText::verified(AuthoredText::new(author, message), signature).is_ok());

        let moved = TextMessage::in_channel(
            MessageId::from_bytes([4; 32]),
            ChannelId::VOICE_ROOM,
            "legacy",
        )
        .unwrap();
        assert!(SignedText::verified(AuthoredText::new(author, moved), signature).is_err());
    }

    #[test]
    fn channel_names_are_bounded_and_clean() {
        assert!(Channel::new(ChannelId::GENERAL, "", ChannelKind::Text).is_err());
        assert!(Channel::new(ChannelId::GENERAL, " padded ", ChannelKind::Text).is_err());
        assert!(
            Channel::new(
                ChannelId::GENERAL,
                "x".repeat(MAX_CHANNEL_NAME_BYTES + 1),
                ChannelKind::Text,
            )
            .is_err()
        );
    }

    #[test]
    fn attachment_names_and_sizes_are_bounded() {
        let id = MessageId::from_bytes([5; 32]);
        assert!(Attachment::new(id, ChannelId::GENERAL, "", vec![1]).is_err());
        assert!(
            Attachment::new(
                id,
                ChannelId::GENERAL,
                "x".repeat(MAX_ATTACHMENT_NAME_BYTES + 1),
                vec![1],
            )
            .is_err()
        );
        assert!(
            Attachment::new(
                id,
                ChannelId::GENERAL,
                "large.bin",
                vec![0; MAX_ATTACHMENT_BYTES + 1],
            )
            .is_err()
        );
    }

    #[test]
    fn display_names_are_bounded_and_clean() {
        assert_eq!(DisplayName::new(""), Err(DisplayNameError::Empty));
        assert_eq!(DisplayName::new(" padded "), Err(DisplayNameError::Invalid));
        assert_eq!(
            DisplayName::new("line\nbreak"),
            Err(DisplayNameError::Invalid)
        );
        assert_eq!(
            DisplayName::new("x".repeat(MAX_DISPLAY_NAME_BYTES + 1)),
            Err(DisplayNameError::TooLong)
        );
        assert_eq!(DisplayName::new("Alice").unwrap().as_str(), "Alice");
    }
}
