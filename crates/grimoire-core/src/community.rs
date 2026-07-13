use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommunityId([u8; 32]);

impl CommunityId {
    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn legacy(owner: MemberId) -> Self {
        let mut input = b"peer-community/legacy-community/v1\0".to_vec();
        input.extend_from_slice(owner.as_bytes());
        Self(*blake3::hash(&input).as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MemberId([u8; 32]);

impl MemberId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberRole {
    Participant,
    Availability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipChange {
    Admit(MemberId),
    AdmitAvailability(MemberId),
    Remove(MemberId),
}

impl MembershipChange {
    pub const fn member(self) -> MemberId {
        match self {
            Self::Admit(member) | Self::AdmitAvailability(member) | Self::Remove(member) => member,
        }
    }

    pub const fn is_admission(self) -> bool {
        !matches!(self, Self::Remove(_))
    }

    pub(crate) const fn is_availability(self) -> bool {
        matches!(self, Self::AdmitAvailability(_))
    }

    const fn discriminator(self) -> u8 {
        match self {
            Self::Remove(_) => 0,
            Self::Admit(_) => 1,
            Self::AdmitAvailability(_) => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MembershipUpdate {
    revision: u64,
    change: MembershipChange,
}

impl MembershipUpdate {
    pub(crate) const fn new(revision: u64, change: MembershipChange) -> Self {
        Self { revision, change }
    }

    pub(crate) const fn revision(self) -> u64 {
        self.revision
    }

    pub(crate) const fn change(self) -> MembershipChange {
        self.change
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SignedMembership {
    update: MembershipUpdate,
    signature: [u8; 64],
}

impl SignedMembership {
    pub(crate) fn sign(
        signing_key: &SigningKey,
        community_id: CommunityId,
        revision: u64,
        change: MembershipChange,
    ) -> Self {
        let update = MembershipUpdate::new(revision, change);
        let signature = signing_key
            .sign(&membership_signing_bytes(community_id, update))
            .to_bytes();
        Self { update, signature }
    }

    pub(crate) fn verified(
        owner: MemberId,
        community_id: CommunityId,
        update: MembershipUpdate,
        signature: [u8; 64],
    ) -> Result<Self, ed25519_dalek::SignatureError> {
        let verifying_key = VerifyingKey::from_bytes(owner.as_bytes())?;
        let current = verifying_key.verify_strict(
            &membership_signing_bytes(community_id, update),
            &Signature::from_bytes(&signature),
        );
        if current.is_err() && community_id == CommunityId::legacy(owner) {
            verifying_key.verify_strict(
                &legacy_membership_signing_bytes(update),
                &Signature::from_bytes(&signature),
            )?;
        } else {
            current?;
        }
        Ok(Self { update, signature })
    }

    pub(crate) const fn update(&self) -> MembershipUpdate {
        self.update
    }

    pub(crate) const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    pub(crate) fn conflict_key(&self) -> [u8; 97] {
        let mut key = [0; 97];
        key[..32].copy_from_slice(self.update.change().member().as_bytes());
        key[32] = self.update.change().discriminator();
        key[33..].copy_from_slice(&self.signature);
        key
    }

    pub(crate) fn head(&self, community_id: CommunityId) -> [u8; 32] {
        let update = self.update;
        let mut bytes = Vec::with_capacity(170);
        bytes.extend_from_slice(b"peer-community/membership-head/v1\0");
        bytes.extend_from_slice(community_id.as_bytes());
        bytes.extend_from_slice(&update.revision().to_be_bytes());
        bytes.extend_from_slice(update.change().member().as_bytes());
        bytes.push(update.change().discriminator());
        bytes.extend_from_slice(&self.signature);
        *blake3::hash(&bytes).as_bytes()
    }
}

fn membership_signing_bytes(community_id: CommunityId, update: MembershipUpdate) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(104);
    bytes.extend_from_slice(b"peer-community/membership/v2\0");
    bytes.extend_from_slice(community_id.as_bytes());
    bytes.extend_from_slice(&update.revision().to_be_bytes());
    bytes.extend_from_slice(update.change().member().as_bytes());
    bytes.push(update.change().discriminator());
    bytes
}

fn legacy_membership_signing_bytes(update: MembershipUpdate) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(72);
    bytes.extend_from_slice(b"peer-community/membership/v1\0");
    bytes.extend_from_slice(&update.revision().to_be_bytes());
    bytes.extend_from_slice(update.change().member().as_bytes());
    bytes.push(update.change().discriminator());
    bytes
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommunityError {
    #[error("only the community owner may change membership")]
    OnlyOwnerMayChangeMembership,
    #[error("the community owner cannot be removed")]
    OwnerCannotBeRemoved,
    #[error("the community owner cannot be an availability peer")]
    OwnerCannotBeAvailability,
    #[error("an identity that held participant keys cannot become an availability peer")]
    ParticipantCannotBecomeAvailability,
    #[error("communication requires admitted community members")]
    MemberNotAdmitted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Community {
    owner: MemberId,
    members: BTreeMap<MemberId, MemberRole>,
    participant_history: BTreeSet<MemberId>,
}

impl Community {
    pub fn new(owner: MemberId) -> Self {
        Self {
            owner,
            members: BTreeMap::from([(owner, MemberRole::Participant)]),
            participant_history: BTreeSet::from([owner]),
        }
    }

    pub const fn owner(&self) -> MemberId {
        self.owner
    }

    pub fn contains(&self, member: MemberId) -> bool {
        self.members.contains_key(&member)
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn members(&self) -> impl Iterator<Item = MemberId> + '_ {
        self.members.keys().copied()
    }

    pub fn role(&self, member: MemberId) -> Option<MemberRole> {
        self.members.get(&member).copied()
    }

    pub fn authorize_participant(&self, member: MemberId) -> Result<(), CommunityError> {
        if self.role(member) == Some(MemberRole::Participant) {
            Ok(())
        } else {
            Err(CommunityError::MemberNotAdmitted)
        }
    }

    pub fn authorize_exchange(
        &self,
        sender: MemberId,
        recipient: MemberId,
    ) -> Result<(), CommunityError> {
        if self.contains(sender) && self.contains(recipient) {
            Ok(())
        } else {
            Err(CommunityError::MemberNotAdmitted)
        }
    }

    pub fn change_membership(
        &mut self,
        actor: MemberId,
        change: MembershipChange,
    ) -> Result<bool, CommunityError> {
        if actor != self.owner {
            return Err(CommunityError::OnlyOwnerMayChangeMembership);
        }

        match change {
            MembershipChange::Admit(member) => {
                self.participant_history.insert(member);
                Ok(self.members.insert(member, MemberRole::Participant)
                    != Some(MemberRole::Participant))
            }
            MembershipChange::AdmitAvailability(member) if member == self.owner => {
                Err(CommunityError::OwnerCannotBeAvailability)
            }
            MembershipChange::AdmitAvailability(member)
                if self.participant_history.contains(&member) =>
            {
                Err(CommunityError::ParticipantCannotBecomeAvailability)
            }
            MembershipChange::AdmitAvailability(member) => {
                Ok(self.members.insert(member, MemberRole::Availability)
                    != Some(MemberRole::Availability))
            }
            MembershipChange::Remove(member) if member == self.owner => {
                Err(CommunityError::OwnerCannotBeRemoved)
            }
            MembershipChange::Remove(member) => Ok(self.members.remove(&member).is_some()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn member(id: u8) -> MemberId {
        MemberId::from_bytes([id; 32])
    }

    #[test]
    fn owner_controls_idempotent_membership_changes() {
        let owner = member(1);
        let alice = member(2);
        let mut community = Community::new(owner);

        assert_eq!(
            community.authorize_exchange(owner, alice),
            Err(CommunityError::MemberNotAdmitted)
        );

        assert_eq!(
            community.change_membership(owner, MembershipChange::Admit(alice)),
            Ok(true)
        );
        assert_eq!(community.authorize_exchange(owner, alice), Ok(()));
        assert_eq!(
            community.change_membership(owner, MembershipChange::Admit(alice)),
            Ok(false)
        );
        assert_eq!(
            community.change_membership(alice, MembershipChange::Remove(owner)),
            Err(CommunityError::OnlyOwnerMayChangeMembership)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::Remove(owner)),
            Err(CommunityError::OwnerCannotBeRemoved)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::Remove(alice)),
            Ok(true)
        );
        assert!(!community.contains(alice));
        assert_eq!(
            community.authorize_participant(alice),
            Err(CommunityError::MemberNotAdmitted)
        );
        assert_eq!(community.member_count(), 1);
    }

    #[test]
    fn availability_role_is_authenticated_and_cannot_participate() {
        let owner_key = SigningKey::from_bytes(&[9; 32]);
        let owner = MemberId::from_bytes(owner_key.verifying_key().to_bytes());
        let availability = member(2);
        let community_id = CommunityId::from_bytes([7; 32]);
        let signed = SignedMembership::sign(
            &owner_key,
            community_id,
            1,
            MembershipChange::AdmitAvailability(availability),
        );

        assert!(
            SignedMembership::verified(
                owner,
                community_id,
                MembershipUpdate::new(1, MembershipChange::Admit(availability)),
                *signed.signature(),
            )
            .is_err()
        );
        assert!(
            SignedMembership::verified(
                owner,
                CommunityId::from_bytes([8; 32]),
                signed.update(),
                *signed.signature(),
            )
            .is_err()
        );

        let mut community = Community::new(owner);
        assert_eq!(
            community.change_membership(owner, MembershipChange::AdmitAvailability(availability),),
            Ok(true)
        );
        assert_eq!(community.role(availability), Some(MemberRole::Availability));
        assert_eq!(community.authorize_exchange(owner, availability), Ok(()));
        assert_eq!(
            community.authorize_participant(availability),
            Err(CommunityError::MemberNotAdmitted)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::AdmitAvailability(owner)),
            Err(CommunityError::OwnerCannotBeAvailability)
        );

        let participant = member(3);
        assert_eq!(
            community.change_membership(owner, MembershipChange::Admit(participant)),
            Ok(true)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::AdmitAvailability(participant)),
            Err(CommunityError::ParticipantCannotBecomeAvailability)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::Remove(participant)),
            Ok(true)
        );
        assert_eq!(
            community.change_membership(owner, MembershipChange::AdmitAvailability(participant)),
            Err(CommunityError::ParticipantCannotBecomeAvailability)
        );
    }
}
