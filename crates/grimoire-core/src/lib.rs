mod community;
mod crypto;
mod identity;
mod metrics;
mod model;
mod node;
mod store;

mod wire {
    include!(concat!(env!("OUT_DIR"), "/peer.v1.rs"));
}

pub use community::{
    Community, CommunityError, CommunityId, MemberId, MemberRole, MembershipChange,
};
pub use identity::restore_identity;
pub use metrics::{Counter, Metrics, MetricsSnapshot};
pub use model::{
    Attachment, AttachmentError, AuthoredAttachment, AuthoredText, AuthoredVoice, Channel,
    ChannelError, ChannelId, ChannelKind, Command, DisplayName, DisplayNameError, Event, Fault,
    FaultKind, MAX_ATTACHMENT_BYTES, MAX_VOICE_PARTICIPANTS, MessageError, MessageId, Snapshot,
    TextMessage, VoiceFrame, VoiceFrameError, VoicePresence, VoiceStreamId,
};
pub use node::{
    CommunityInvite, CommunityInviteError, ConnectionPathDiagnostic, ConnectionPathKind, Node,
    NodeConfig, PeerAddress, PeerAddressError, PeerDiagnostic,
};

pub type NodeError = Fault;
