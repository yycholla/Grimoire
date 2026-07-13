use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hpke::{
    Deserializable, Kem as _, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305,
    kdf::HkdfSha256, kem::X25519HkdfSha256,
};
use prost::Message;
use rand::{SeedableRng, rngs::StdRng};

use crate::{
    Attachment, AuthoredAttachment, AuthoredText, ChannelId, CommunityId, DisplayName, MemberId,
    MessageId, NodeError, TextMessage, VoicePresence, wire,
};

type Kem = X25519HkdfSha256;
type HpkeAead = ChaCha20Poly1305;
type HpkeKdf = HkdfSha256;

#[derive(Clone, Debug)]
pub(crate) struct KeyRegistration {
    member: MemberId,
    public_key: [u8; 32],
    signature: [u8; 64],
}

impl KeyRegistration {
    pub(crate) fn sign(
        signing_key: &SigningKey,
        community_id: CommunityId,
        public_key: [u8; 32],
    ) -> Self {
        let member = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        let signature = signing_key
            .sign(&registration_bytes(community_id, member, &public_key))
            .to_bytes();
        Self {
            member,
            public_key,
            signature,
        }
    }

    pub(crate) fn verified(
        community_id: CommunityId,
        member: MemberId,
        public_key: [u8; 32],
        signature: [u8; 64],
    ) -> Result<Self, NodeError> {
        VerifyingKey::from_bytes(member.as_bytes())
            .and_then(|key| {
                key.verify_strict(
                    &registration_bytes(community_id, member, &public_key),
                    &Signature::from_bytes(&signature),
                )
            })
            .map_err(NodeError::protocol)?;
        Ok(Self {
            member,
            public_key,
            signature,
        })
    }

    pub(crate) const fn member(&self) -> MemberId {
        self.member
    }
    pub(crate) const fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }
    pub(crate) const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }
}

fn registration_bytes(id: CommunityId, member: MemberId, public_key: &[u8; 32]) -> Vec<u8> {
    [
        b"peer-community/key-registration/v1\0".as_slice(),
        id.as_bytes(),
        member.as_bytes(),
        public_key,
    ]
    .concat()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ContentEpoch {
    pub(crate) number: u64,
    pub(crate) head: [u8; 32],
    pub(crate) key: [u8; 32],
}

#[derive(Clone, Debug)]
pub(crate) struct ContentKeyEnvelope {
    pub(crate) epoch: u64,
    pub(crate) head: [u8; 32],
    pub(crate) recipient: MemberId,
    pub(crate) encapsulated_key: [u8; 32],
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) signature: [u8; 64],
}

impl ContentKeyEnvelope {
    pub(crate) fn seal(
        owner: &SigningKey,
        community_id: CommunityId,
        epoch: ContentEpoch,
        registration: &KeyRegistration,
    ) -> Result<Self, NodeError> {
        let public_key = <Kem as hpke::Kem>::PublicKey::from_bytes(registration.public_key())
            .map_err(NodeError::protocol)?;
        let info = envelope_info(
            community_id,
            epoch.number,
            &epoch.head,
            registration.member(),
        );
        let mut rng = StdRng::from_os_rng();
        let (encapsulated_key, mut context) = hpke::setup_sender::<HpkeAead, HpkeKdf, Kem, _>(
            &OpModeS::Base,
            &public_key,
            &info,
            &mut rng,
        )
        .map_err(NodeError::protocol)?;
        let ciphertext = context
            .seal(&epoch.key, &info)
            .map_err(NodeError::protocol)?;
        let encapsulated_key: [u8; 32] = encapsulated_key
            .to_bytes()
            .as_slice()
            .try_into()
            .map_err(|_| NodeError::protocol("HPKE encapsulated key is not 32 bytes"))?;
        let mut envelope = Self {
            epoch: epoch.number,
            head: epoch.head,
            recipient: registration.member(),
            encapsulated_key,
            ciphertext,
            signature: [0; 64],
        };
        envelope.signature = owner.sign(&envelope.signing_bytes(community_id)).to_bytes();
        Ok(envelope)
    }

    pub(crate) fn verify(&self, owner: MemberId, id: CommunityId) -> Result<(), NodeError> {
        VerifyingKey::from_bytes(owner.as_bytes())
            .map_err(NodeError::protocol)?
            .verify_strict(
                &self.signing_bytes(id),
                &Signature::from_bytes(&self.signature),
            )
            .map_err(NodeError::protocol)
    }

    pub(crate) fn open(
        &self,
        id: CommunityId,
        hpke_seed: &[u8; 32],
    ) -> Result<ContentEpoch, NodeError> {
        let (private_key, _) = Kem::derive_keypair(hpke_seed);
        let encapsulated_key = <Kem as hpke::Kem>::EncappedKey::from_bytes(&self.encapsulated_key)
            .map_err(NodeError::protocol)?;
        let info = envelope_info(id, self.epoch, &self.head, self.recipient);
        let mut context = hpke::setup_receiver::<HpkeAead, HpkeKdf, Kem>(
            &OpModeR::Base,
            &private_key,
            &encapsulated_key,
            &info,
        )
        .map_err(NodeError::protocol)?;
        let key: [u8; 32] = context
            .open(&self.ciphertext, &info)
            .map_err(NodeError::protocol)?
            .try_into()
            .map_err(|_| NodeError::protocol("content key is not 32 bytes"))?;
        Ok(ContentEpoch {
            number: self.epoch,
            head: self.head,
            key,
        })
    }

    fn signing_bytes(&self, id: CommunityId) -> Vec<u8> {
        let mut bytes = envelope_info(id, self.epoch, &self.head, self.recipient);
        bytes.extend_from_slice(&self.encapsulated_key);
        bytes.extend_from_slice(&(self.ciphertext.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&self.ciphertext);
        bytes
    }
}

fn envelope_info(id: CommunityId, epoch: u64, head: &[u8; 32], recipient: MemberId) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(128);
    bytes.extend_from_slice(b"peer-community/content-key-envelope/v1\0");
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(recipient.as_bytes());
    bytes
}

pub(crate) fn hpke_public(seed: &[u8; 32]) -> [u8; 32] {
    let (_, public) = Kem::derive_keypair(seed);
    public
        .to_bytes()
        .as_slice()
        .try_into()
        .expect("X25519 public key is 32 bytes")
}

pub(crate) fn genesis_head(id: CommunityId, owner: MemberId) -> [u8; 32] {
    *blake3::hash(
        &[
            b"peer-community/membership-genesis/v1\0".as_slice(),
            id.as_bytes(),
            owner.as_bytes(),
        ]
        .concat(),
    )
    .as_bytes()
}

#[derive(Clone, Debug)]
pub(crate) struct EncryptedText {
    pub(crate) id: MessageId,
    pub(crate) channel_id: crate::ChannelId,
    pub(crate) author: MemberId,
    pub(crate) epoch: u64,
    pub(crate) head: [u8; 32],
    pub(crate) nonce: [u8; 24],
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) signature: [u8; 64],
}

impl EncryptedText {
    pub(crate) fn encrypt(
        signing_key: &SigningKey,
        id: CommunityId,
        epoch: ContentEpoch,
        message: TextMessage,
    ) -> Result<Self, NodeError> {
        let author = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        let nonce: [u8; 24] = rand::random();
        let aad = text_aad(
            id,
            epoch.number,
            &epoch.head,
            author,
            message.id(),
            message.channel_id(),
        );
        let ciphertext = XChaCha20Poly1305::new((&epoch.key).into())
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: message.body().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        let signature = signing_key
            .sign(&encrypted_text_signing_bytes(
                id,
                epoch.number,
                &epoch.head,
                author,
                message.id(),
                message.channel_id(),
                &nonce,
                &ciphertext,
            ))
            .to_bytes();
        Ok(Self {
            id: message.id(),
            channel_id: message.channel_id(),
            author,
            epoch: epoch.number,
            head: epoch.head,
            nonce,
            ciphertext,
            signature,
        })
    }

    pub(crate) fn decrypt(
        &self,
        id: CommunityId,
        key: &[u8; 32],
    ) -> Result<AuthoredText, NodeError> {
        self.verify(id)?;
        let aad = text_aad(
            id,
            self.epoch,
            &self.head,
            self.author,
            self.id,
            self.channel_id,
        );
        let body = XChaCha20Poly1305::new(key.into())
            .decrypt(
                (&self.nonce).into(),
                Payload {
                    msg: &self.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        let body = String::from_utf8(body).map_err(NodeError::protocol)?;
        let message = TextMessage::in_channel(self.id, self.channel_id, body)?;
        Ok(AuthoredText::new(self.author, message))
    }

    pub(crate) fn verify(&self, id: CommunityId) -> Result<(), NodeError> {
        VerifyingKey::from_bytes(self.author.as_bytes())
            .map_err(NodeError::protocol)?
            .verify_strict(
                &encrypted_text_signing_bytes(
                    id,
                    self.epoch,
                    &self.head,
                    self.author,
                    self.id,
                    self.channel_id,
                    &self.nonce,
                    &self.ciphertext,
                ),
                &Signature::from_bytes(&self.signature),
            )
            .map_err(NodeError::protocol)
    }
}

fn text_aad(
    id: CommunityId,
    epoch: u64,
    head: &[u8; 32],
    author: MemberId,
    message_id: MessageId,
    channel: crate::ChannelId,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(180);
    bytes.extend_from_slice(b"peer-community/text-aead/v1\0");
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(author.as_bytes());
    bytes.extend_from_slice(message_id.as_bytes());
    bytes.extend_from_slice(channel.as_bytes());
    bytes
}

#[allow(clippy::too_many_arguments)]
fn encrypted_text_signing_bytes(
    id: CommunityId,
    epoch: u64,
    head: &[u8; 32],
    author: MemberId,
    message_id: MessageId,
    channel_id: crate::ChannelId,
    nonce: &[u8; 24],
    ciphertext: &[u8],
) -> Vec<u8> {
    let mut bytes = text_aad(id, epoch, head, author, message_id, channel_id);
    bytes[0..b"peer-community/text-aead/v1\0".len()]
        .copy_from_slice(b"peer-community/text-sign/v1\0");
    bytes.extend_from_slice(nonce);
    bytes.extend_from_slice(&(ciphertext.len() as u64).to_be_bytes());
    bytes.extend_from_slice(ciphertext);
    bytes
}

#[derive(Clone, Debug)]
pub(crate) struct EncryptedMemberProfile {
    pub(crate) member: MemberId,
    pub(crate) revision: u64,
    pub(crate) epoch: u64,
    pub(crate) head: [u8; 32],
    pub(crate) nonce: [u8; 24],
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) signature: [u8; 64],
}

impl EncryptedMemberProfile {
    pub(crate) fn encrypt(
        signing_key: &SigningKey,
        id: CommunityId,
        epoch: ContentEpoch,
        revision: u64,
        name: DisplayName,
    ) -> Result<Self, NodeError> {
        let member = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        let nonce: [u8; 24] = rand::random();
        let aad = member_profile_aad(id, member, revision, epoch.number, &epoch.head);
        let ciphertext = XChaCha20Poly1305::new((&epoch.key).into())
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: name.as_str().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        let signature = signing_key
            .sign(&member_profile_signing_bytes(&aad, &nonce, &ciphertext))
            .to_bytes();
        Ok(Self {
            member,
            revision,
            epoch: epoch.number,
            head: epoch.head,
            nonce,
            ciphertext,
            signature,
        })
    }

    pub(crate) fn verify(&self, id: CommunityId) -> Result<(), NodeError> {
        let aad = member_profile_aad(id, self.member, self.revision, self.epoch, &self.head);
        VerifyingKey::from_bytes(self.member.as_bytes())
            .map_err(NodeError::protocol)?
            .verify_strict(
                &member_profile_signing_bytes(&aad, &self.nonce, &self.ciphertext),
                &Signature::from_bytes(&self.signature),
            )
            .map_err(NodeError::protocol)
    }

    pub(crate) fn decrypt(
        &self,
        id: CommunityId,
        key: &[u8; 32],
    ) -> Result<DisplayName, NodeError> {
        self.verify(id)?;
        let aad = member_profile_aad(id, self.member, self.revision, self.epoch, &self.head);
        let plaintext = XChaCha20Poly1305::new(key.into())
            .decrypt(
                (&self.nonce).into(),
                Payload {
                    msg: &self.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        DisplayName::new(String::from_utf8(plaintext).map_err(NodeError::protocol)?)
            .map_err(NodeError::from)
    }
}

fn member_profile_aad(
    id: CommunityId,
    member: MemberId,
    revision: u64,
    epoch: u64,
    head: &[u8; 32],
) -> Vec<u8> {
    [
        b"peer-community/member-profile-aead/v1\0".as_slice(),
        id.as_bytes(),
        member.as_bytes(),
        &revision.to_be_bytes(),
        &epoch.to_be_bytes(),
        head,
    ]
    .concat()
}

fn member_profile_signing_bytes(aad: &[u8], nonce: &[u8; 24], ciphertext: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(aad.len() + nonce.len() + ciphertext.len() + 8);
    bytes.extend_from_slice(aad);
    bytes.extend_from_slice(nonce);
    bytes.extend_from_slice(&(ciphertext.len() as u64).to_be_bytes());
    bytes.extend_from_slice(ciphertext);
    bytes
}

#[derive(Clone, Debug)]
pub(crate) struct EncryptedAttachment {
    pub(crate) id: MessageId,
    pub(crate) channel_id: crate::ChannelId,
    pub(crate) author: MemberId,
    pub(crate) epoch: u64,
    pub(crate) head: [u8; 32],
    pub(crate) nonce: [u8; 24],
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) signature: [u8; 64],
}

impl EncryptedAttachment {
    pub(crate) fn encrypt(
        signing_key: &SigningKey,
        community_id: CommunityId,
        epoch: ContentEpoch,
        attachment: Attachment,
    ) -> Result<Self, NodeError> {
        let author = MemberId::from_bytes(signing_key.verifying_key().to_bytes());
        let nonce: [u8; 24] = rand::random();
        let aad = attachment_aad(
            community_id,
            epoch.number,
            &epoch.head,
            author,
            attachment.id(),
            attachment.channel_id(),
        );
        let mut plaintext =
            Vec::with_capacity(2 + attachment.name().len() + attachment.bytes().len());
        plaintext.extend_from_slice(&(attachment.name().len() as u16).to_be_bytes());
        plaintext.extend_from_slice(attachment.name().as_bytes());
        plaintext.extend_from_slice(attachment.bytes());
        let ciphertext = XChaCha20Poly1305::new((&epoch.key).into())
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &plaintext,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        let digest = *blake3::hash(&ciphertext).as_bytes();
        let signature = signing_key
            .sign(&attachment_signing_bytes(&aad, &nonce, &digest))
            .to_bytes();
        Ok(Self {
            id: attachment.id(),
            channel_id: attachment.channel_id(),
            author,
            epoch: epoch.number,
            head: epoch.head,
            nonce,
            ciphertext,
            signature,
        })
    }

    pub(crate) fn verify(&self, community_id: CommunityId) -> Result<(), NodeError> {
        let aad = attachment_aad(
            community_id,
            self.epoch,
            &self.head,
            self.author,
            self.id,
            self.channel_id,
        );
        let digest = *blake3::hash(&self.ciphertext).as_bytes();
        VerifyingKey::from_bytes(self.author.as_bytes())
            .map_err(NodeError::protocol)?
            .verify_strict(
                &attachment_signing_bytes(&aad, &self.nonce, &digest),
                &Signature::from_bytes(&self.signature),
            )
            .map_err(NodeError::protocol)
    }

    pub(crate) fn decrypt(
        &self,
        community_id: CommunityId,
        key: &[u8; 32],
    ) -> Result<AuthoredAttachment, NodeError> {
        self.verify(community_id)?;
        let aad = attachment_aad(
            community_id,
            self.epoch,
            &self.head,
            self.author,
            self.id,
            self.channel_id,
        );
        let plaintext = XChaCha20Poly1305::new(key.into())
            .decrypt(
                (&self.nonce).into(),
                Payload {
                    msg: &self.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        if plaintext.len() < 2 {
            return Err(NodeError::protocol("attachment plaintext is truncated"));
        }
        let name_len = usize::from(u16::from_be_bytes([plaintext[0], plaintext[1]]));
        let name_end = 2usize
            .checked_add(name_len)
            .ok_or_else(|| NodeError::protocol("attachment name length overflow"))?;
        let name = std::str::from_utf8(
            plaintext
                .get(2..name_end)
                .ok_or_else(|| NodeError::protocol("attachment name is truncated"))?,
        )
        .map_err(NodeError::protocol)?;
        let bytes = plaintext
            .get(name_end..)
            .ok_or_else(|| NodeError::protocol("attachment payload is truncated"))?
            .to_vec();
        Ok(AuthoredAttachment::new(
            self.author,
            Attachment::new(self.id, self.channel_id, name, bytes)?,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn attachment_aad(
    community_id: CommunityId,
    epoch: u64,
    head: &[u8; 32],
    author: MemberId,
    id: MessageId,
    channel_id: crate::ChannelId,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(200);
    bytes.extend_from_slice(b"peer-community/attachment-aead/v1\0");
    bytes.extend_from_slice(community_id.as_bytes());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(author.as_bytes());
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(channel_id.as_bytes());
    bytes
}

fn attachment_signing_bytes(aad: &[u8], nonce: &[u8; 24], digest: &[u8; 32]) -> Vec<u8> {
    let mut bytes = aad.to_vec();
    bytes[..b"peer-community/attachment-aead/v1\0".len()]
        .copy_from_slice(b"peer-community/attachment-sign/v1\0");
    bytes.extend_from_slice(nonce);
    bytes.extend_from_slice(digest);
    bytes
}

pub(crate) fn encrypt_voice(
    id: CommunityId,
    epoch: ContentEpoch,
    author: MemberId,
    frame: &crate::VoiceFrame,
) -> Result<([u8; 24], Vec<u8>), NodeError> {
    let nonce: [u8; 24] = rand::random();
    let aad = voice_aad(id, epoch.number, &epoch.head, author, frame);
    let ciphertext = XChaCha20Poly1305::new((&epoch.key).into())
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: frame.payload(),
                aad: &aad,
            },
        )
        .map_err(NodeError::protocol)?;
    Ok((nonce, ciphertext))
}

pub(crate) fn decrypt_voice(
    id: CommunityId,
    epoch: ContentEpoch,
    author: MemberId,
    frame: &crate::VoiceFrame,
    nonce: &[u8; 24],
) -> Result<Vec<u8>, NodeError> {
    let aad = voice_aad(id, epoch.number, &epoch.head, author, frame);
    XChaCha20Poly1305::new((&epoch.key).into())
        .decrypt(
            nonce.into(),
            Payload {
                msg: frame.payload(),
                aad: &aad,
            },
        )
        .map_err(NodeError::protocol)
}

#[derive(Clone, Debug)]
pub(crate) struct EncryptedVoicePresence {
    pub(crate) epoch: u64,
    pub(crate) head: [u8; 32],
    pub(crate) nonce: [u8; 24],
    pub(crate) ciphertext: Vec<u8>,
}

impl EncryptedVoicePresence {
    pub(crate) fn encrypt(
        id: CommunityId,
        epoch: ContentEpoch,
        author: MemberId,
        channel: ChannelId,
        state: VoicePresence,
    ) -> Result<Self, NodeError> {
        let payload = wire::VoicePresencePayload {
            channel_id: channel.as_bytes().to_vec(),
            state: Some(match state {
                VoicePresence::Joined => wire::voice_presence_payload::State::Joined(true),
                VoicePresence::Left => wire::voice_presence_payload::State::Left(true),
                VoicePresence::Muted(muted) => wire::voice_presence_payload::State::Muted(muted),
            }),
        }
        .encode_to_vec();
        let nonce: [u8; 24] = rand::random();
        let aad = voice_presence_aad(id, epoch.number, &epoch.head, author);
        let ciphertext = XChaCha20Poly1305::new((&epoch.key).into())
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &payload,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        Ok(Self {
            epoch: epoch.number,
            head: epoch.head,
            nonce,
            ciphertext,
        })
    }

    pub(crate) fn decrypt(
        &self,
        id: CommunityId,
        author: MemberId,
        key: &[u8; 32],
    ) -> Result<(ChannelId, VoicePresence), NodeError> {
        let aad = voice_presence_aad(id, self.epoch, &self.head, author);
        let plaintext = XChaCha20Poly1305::new(key.into())
            .decrypt(
                (&self.nonce).into(),
                Payload {
                    msg: &self.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(NodeError::protocol)?;
        let payload = wire::VoicePresencePayload::decode(plaintext.as_slice())?;
        let channel = ChannelId::from_bytes(
            payload
                .channel_id
                .try_into()
                .map_err(|_| NodeError::protocol("voice presence channel is not 32 bytes"))?,
        );
        let state = match payload
            .state
            .ok_or_else(|| NodeError::protocol("voice presence has no state"))?
        {
            wire::voice_presence_payload::State::Joined(true) => VoicePresence::Joined,
            wire::voice_presence_payload::State::Left(true) => VoicePresence::Left,
            wire::voice_presence_payload::State::Muted(muted) => VoicePresence::Muted(muted),
            wire::voice_presence_payload::State::Joined(false)
            | wire::voice_presence_payload::State::Left(false) => {
                return Err(NodeError::protocol("voice presence state is invalid"));
            }
        };
        Ok((channel, state))
    }
}

fn voice_presence_aad(id: CommunityId, epoch: u64, head: &[u8; 32], author: MemberId) -> Vec<u8> {
    [
        b"peer-community/voice-presence/v1\0".as_slice(),
        id.as_bytes(),
        &epoch.to_be_bytes(),
        head,
        author.as_bytes(),
    ]
    .concat()
}

fn voice_aad(
    id: CommunityId,
    epoch: u64,
    head: &[u8; 32],
    author: MemberId,
    frame: &crate::VoiceFrame,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(180);
    bytes.extend_from_slice(b"peer-community/voice-aead/v1\0");
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(author.as_bytes());
    bytes.extend_from_slice(frame.stream_id().as_bytes());
    bytes.extend_from_slice(frame.channel_id().as_bytes());
    bytes.extend_from_slice(&frame.sequence().to_be_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelId, MessageId};

    #[test]
    fn encrypted_text_binds_ciphertext_and_community() {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let community = CommunityId::from_bytes([1; 32]);
        let epoch = ContentEpoch {
            number: 3,
            head: [2; 32],
            key: [3; 32],
        };
        let message =
            TextMessage::in_channel(MessageId::from_bytes([4; 32]), ChannelId::GENERAL, "secret")
                .unwrap();
        let encrypted =
            EncryptedText::encrypt(&signing, community, epoch, message.clone()).unwrap();
        assert_eq!(
            encrypted.decrypt(community, &epoch.key).unwrap().message(),
            &message
        );
        assert!(
            encrypted
                .decrypt(CommunityId::from_bytes([9; 32]), &epoch.key)
                .is_err()
        );
        let mut tampered = encrypted;
        tampered.ciphertext[0] ^= 1;
        assert!(tampered.decrypt(community, &epoch.key).is_err());
    }

    #[test]
    fn hpke_envelope_opens_only_for_registered_recipient() {
        let owner = SigningKey::from_bytes(&[8; 32]);
        let recipient = SigningKey::from_bytes(&[9; 32]);
        let community = CommunityId::from_bytes([5; 32]);
        let recipient_seed = [6; 32];
        let registration =
            KeyRegistration::sign(&recipient, community, hpke_public(&recipient_seed));
        let epoch = ContentEpoch {
            number: 1,
            head: [7; 32],
            key: [8; 32],
        };
        let envelope = ContentKeyEnvelope::seal(&owner, community, epoch, &registration).unwrap();
        envelope
            .verify(
                MemberId::from_bytes(owner.verifying_key().to_bytes()),
                community,
            )
            .unwrap();
        assert_eq!(
            envelope.open(community, &recipient_seed).unwrap().key,
            epoch.key
        );
        assert!(envelope.open(community, &[1; 32]).is_err());
    }

    #[test]
    fn voice_presence_hides_channel_and_binds_the_connection_author() {
        let community = CommunityId::from_bytes([1; 32]);
        let author = MemberId::from_bytes([2; 32]);
        let epoch = ContentEpoch {
            number: 3,
            head: [4; 32],
            key: [5; 32],
        };
        let channel = ChannelId::from_bytes([6; 32]);
        let encrypted = EncryptedVoicePresence::encrypt(
            community,
            epoch,
            author,
            channel,
            VoicePresence::Muted(true),
        )
        .unwrap();

        assert!(
            !encrypted
                .ciphertext
                .windows(channel.as_bytes().len())
                .any(|window| window == channel.as_bytes())
        );
        assert_eq!(
            encrypted.decrypt(community, author, &epoch.key).unwrap(),
            (channel, VoicePresence::Muted(true))
        );
        assert!(
            encrypted
                .decrypt(community, MemberId::from_bytes([9; 32]), &epoch.key)
                .is_err()
        );
        let mut tampered = encrypted;
        tampered.ciphertext[0] ^= 1;
        assert!(tampered.decrypt(community, author, &epoch.key).is_err());
    }
}
