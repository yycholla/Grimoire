use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use crate::{
    AuthoredAttachment, AuthoredText, Channel, ChannelId, ChannelKind, Community, CommunityId,
    MemberId, MembershipChange, MessageId, NodeError, Snapshot, TextMessage,
    community::{MembershipUpdate, SignedMembership},
    crypto::{
        ContentEpoch, ContentKeyEnvelope, EncryptedAttachment, EncryptedMemberProfile,
        EncryptedText, KeyRegistration, genesis_head,
    },
    identity::IdentityMaterial,
    model::{SignedCreateChannel, SignedText},
};

#[derive(Debug)]
pub(crate) struct Store {
    database: turso::Database,
    membership_mutation: tokio::sync::Mutex<()>,
    channel_mutation: tokio::sync::Mutex<()>,
    member_profile_mutation: tokio::sync::Mutex<()>,
}

impl Store {
    pub async fn open(data_dir: &Path) -> Result<Self, NodeError> {
        tokio::fs::create_dir_all(data_dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let path = data_dir.join("peer.db");
        let database = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await?;
        let store = Self {
            database,
            membership_mutation: tokio::sync::Mutex::new(()),
            channel_mutation: tokio::sync::Mutex::new(()),
            member_profile_mutation: tokio::sync::Mutex::new(()),
        };
        let connection = store.database.connect()?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS messages (\
                    id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32), \
                    body TEXT NOT NULL, \
                    author BLOB CHECK(author IS NULL OR length(author) = 32), \
                    signature BLOB CHECK(signature IS NULL OR length(signature) = 64)\
                    , channel_id BLOB NOT NULL CHECK(length(channel_id) = 32)\
                )",
                (),
            )
            .await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS member_profiles (\
                    member BLOB PRIMARY KEY NOT NULL CHECK(length(member) = 32), \
                    revision INTEGER NOT NULL CHECK(revision > 0), \
                    epoch INTEGER NOT NULL, \
                    membership_head BLOB NOT NULL CHECK(length(membership_head) = 32), \
                    nonce BLOB NOT NULL CHECK(length(nonce) = 24), \
                    ciphertext BLOB NOT NULL, \
                    signature BLOB NOT NULL CHECK(length(signature) = 64)\
                )",
                (),
            )
            .await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS encrypted_messages (\
                    id BLOB NOT NULL CHECK(length(id) = 32), \
                    author BLOB NOT NULL CHECK(length(author) = 32), \
                    channel_id BLOB NOT NULL CHECK(length(channel_id) = 32), \
                    epoch INTEGER NOT NULL, \
                    membership_head BLOB NOT NULL CHECK(length(membership_head) = 32), \
                    nonce BLOB NOT NULL CHECK(length(nonce) = 24), \
                    ciphertext BLOB NOT NULL, \
                    signature BLOB NOT NULL CHECK(length(signature) = 64), \
                    PRIMARY KEY(author, id)\
                )",
                (),
            )
            .await?;
        connection.execute(
            "CREATE TABLE IF NOT EXISTS encrypted_attachments (\
                id BLOB NOT NULL CHECK(length(id) = 32), author BLOB NOT NULL CHECK(length(author) = 32), \
                channel_id BLOB NOT NULL CHECK(length(channel_id) = 32), epoch INTEGER NOT NULL, \
                membership_head BLOB NOT NULL CHECK(length(membership_head) = 32), nonce BLOB NOT NULL CHECK(length(nonce) = 24), \
                ciphertext BLOB NOT NULL, signature BLOB NOT NULL CHECK(length(signature) = 64), PRIMARY KEY(author, id)\
            )", (),
        ).await?;
        connection.execute(
            "CREATE TABLE IF NOT EXISTS forgotten_attachments (author BLOB NOT NULL CHECK(length(author) = 32), id BLOB NOT NULL CHECK(length(id) = 32), PRIMARY KEY(author, id))",
            (),
        ).await?;
        ensure_table_column(&connection, "messages", "author", "BLOB").await?;
        ensure_table_column(&connection, "messages", "signature", "BLOB").await?;
        ensure_table_column(&connection, "messages", "channel_id", "BLOB").await?;
        connection
            .execute(
                "UPDATE messages SET channel_id = ?1 WHERE channel_id IS NULL",
                [ChannelId::GENERAL.as_bytes().as_slice()],
            )
            .await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS identity (\
                    singleton INTEGER PRIMARY KEY CHECK(singleton = 1), \
                    seed BLOB NOT NULL CHECK(length(seed) = 32), \
                    hpke_seed BLOB CHECK(hpke_seed IS NULL OR length(hpke_seed) = 32)\
                )",
                (),
            )
            .await?;
        ensure_table_column(&connection, "identity", "hpke_seed", "BLOB").await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS community (\
                    singleton INTEGER PRIMARY KEY CHECK(singleton = 1), \
                    owner BLOB NOT NULL CHECK(length(owner) = 32), \
                    id BLOB CHECK(id IS NULL OR length(id) = 32), \
                    recovered INTEGER NOT NULL DEFAULT 0 CHECK(recovered IN (0, 1))\
                )",
                (),
            )
            .await?;
        ensure_table_column(&connection, "community", "id", "BLOB").await?;
        ensure_table_column(
            &connection,
            "community",
            "recovered",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS membership_changes (\
                    revision INTEGER PRIMARY KEY CHECK(revision > 0), \
                    member BLOB NOT NULL CHECK(length(member) = 32), \
                    admitted INTEGER NOT NULL CHECK(admitted IN (0, 1)), \
                    availability INTEGER NOT NULL DEFAULT 0 CHECK(availability IN (0, 1)), \
                    signature BLOB NOT NULL CHECK(length(signature) = 64), \
                    conflict_key BLOB NOT NULL CHECK(length(conflict_key) = 97)\
                )",
                (),
            )
            .await?;
        ensure_table_column(
            &connection,
            "membership_changes",
            "availability",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        for (name, definition) in [
            ("epoch", "INTEGER"),
            ("membership_head", "BLOB"),
            ("nonce", "BLOB"),
            ("ciphertext", "BLOB"),
        ] {
            ensure_table_column(&connection, "messages", name, definition).await?;
        }
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS key_registrations (\
                member BLOB PRIMARY KEY NOT NULL CHECK(length(member) = 32), \
                public_key BLOB NOT NULL CHECK(length(public_key) = 32), \
                signature BLOB NOT NULL CHECK(length(signature) = 64)\
            )",
                (),
            )
            .await?;
        connection.execute(
            "CREATE TABLE IF NOT EXISTS content_key_envelopes (\
                epoch INTEGER NOT NULL, membership_head BLOB NOT NULL CHECK(length(membership_head) = 32), \
                recipient BLOB NOT NULL CHECK(length(recipient) = 32), \
                encapsulated_key BLOB NOT NULL CHECK(length(encapsulated_key) = 32), \
                ciphertext BLOB NOT NULL, signature BLOB NOT NULL CHECK(length(signature) = 64), \
                PRIMARY KEY(epoch, membership_head, recipient)\
            )", (),
        ).await?;
        connection.execute(
            "CREATE TABLE IF NOT EXISTS content_keys (\
                epoch INTEGER NOT NULL, membership_head BLOB NOT NULL CHECK(length(membership_head) = 32), \
                key BLOB NOT NULL CHECK(length(key) = 32), PRIMARY KEY(epoch, membership_head)\
            )", (),
        ).await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS channels (\
                    id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 32), \
                    name TEXT NOT NULL, \
                    kind INTEGER NOT NULL CHECK(kind IN (0, 1)), \
                    signature BLOB NOT NULL CHECK(length(signature) = 64)\
                )",
                (),
            )
            .await?;
        let had_local_read_state = table_exists(&connection, "local_read_state").await?;
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS local_read_state (\
                    channel_id BLOB PRIMARY KEY NOT NULL CHECK(length(channel_id) = 32), \
                    message_id BLOB NOT NULL CHECK(length(message_id) = 32)\
                )",
                (),
            )
            .await?;
        if !had_local_read_state {
            connection
                .execute(
                    "INSERT INTO local_read_state (channel_id, message_id) \
                     SELECT channel_id, MAX(id) FROM (\
                         SELECT channel_id, id FROM messages \
                         WHERE author IS NOT NULL AND signature IS NOT NULL AND ciphertext IS NULL \
                         UNION ALL SELECT channel_id, id FROM encrypted_messages \
                         UNION ALL SELECT channel_id, id FROM encrypted_attachments AS attachment \
                         WHERE NOT EXISTS (SELECT 1 FROM forgotten_attachments AS forgotten \
                             WHERE forgotten.author = attachment.author AND forgotten.id = attachment.id)\
                     ) GROUP BY channel_id",
                    (),
                )
                .await?;
        }
        Ok(store)
    }

    pub async fn identity_seed(&self) -> Result<[u8; 32], NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query("SELECT seed FROM identity WHERE singleton = 1", ())
            .await?;
        if let Some(row) = rows.next().await? {
            let seed: Vec<u8> = row.get(0)?;
            return seed
                .try_into()
                .map_err(|_| NodeError::protocol("stored identity seed is not 32 bytes"));
        }
        drop(rows);
        let seed: [u8; 32] = rand::random();
        connection
            .execute(
                "INSERT INTO identity (singleton, seed) VALUES (1, ?1)",
                [seed.as_slice()],
            )
            .await?;
        Ok(seed)
    }

    pub async fn hpke_seed(&self) -> Result<[u8; 32], NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query("SELECT hpke_seed FROM identity WHERE singleton = 1", ())
            .await?;
        if let Some(row) = rows.next().await? {
            let seed = row.get::<Option<Vec<u8>>>(0)?;
            drop(rows);
            if let Some(seed) = seed {
                return seed
                    .try_into()
                    .map_err(|_| NodeError::protocol("stored HPKE seed is not 32 bytes"));
            }
        } else {
            drop(rows);
        }
        let seed: [u8; 32] = rand::random();
        connection
            .execute(
                "UPDATE identity SET hpke_seed = ?1 WHERE singleton = 1",
                [seed.as_slice()],
            )
            .await?;
        Ok(seed)
    }

    pub async fn identity_material(&self) -> Result<IdentityMaterial, NodeError> {
        Ok(IdentityMaterial {
            identity_seed: self.identity_seed().await?,
            hpke_seed: self.hpke_seed().await?,
            community_id: self.community_id().await?,
            owner: self.community_owner().await?,
            active_epoch: self.latest_content_epoch().await?,
        })
    }

    pub async fn restore_identity(&self, material: &IdentityMaterial) -> Result<(), NodeError> {
        let mut connection = self.database.connect()?;
        let transaction = connection
            .transaction_with_behavior(turso::transaction::TransactionBehavior::Immediate)
            .await?;
        let mut rows = transaction
            .query(
                "SELECT \
                    (SELECT COUNT(*) FROM identity) + \
                    (SELECT COUNT(*) FROM community) + \
                    (SELECT COUNT(*) FROM membership_changes) + \
                    (SELECT COUNT(*) FROM channels) + \
                    (SELECT COUNT(*) FROM messages) + \
                    (SELECT COUNT(*) FROM encrypted_messages) + \
                    (SELECT COUNT(*) FROM encrypted_attachments) + \
                    (SELECT COUNT(*) FROM forgotten_attachments) + \
                    (SELECT COUNT(*) FROM member_profiles) + \
                    (SELECT COUNT(*) FROM key_registrations) + \
                    (SELECT COUNT(*) FROM content_key_envelopes) + \
                    (SELECT COUNT(*) FROM content_keys) + \
                    (SELECT COUNT(*) FROM local_read_state)",
                (),
            )
            .await?;
        let state_rows: i64 = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("identity restore state query returned no row"))?
            .get(0)?;
        drop(rows);
        if state_rows != 0 {
            return Err(NodeError::authorization(
                "identity recovery requires an empty data directory",
            ));
        }
        transaction
            .execute(
                "INSERT INTO identity (singleton, seed, hpke_seed) VALUES (1, ?1, ?2)",
                turso::params![
                    material.identity_seed.as_slice(),
                    material.hpke_seed.as_slice()
                ],
            )
            .await?;
        transaction
            .execute(
                "INSERT INTO community (singleton, owner, id, recovered) VALUES (1, ?1, ?2, 1)",
                turso::params![
                    material.owner.as_bytes().as_slice(),
                    material.community_id.as_bytes().as_slice()
                ],
            )
            .await?;
        if let Some(active_epoch) = material.active_epoch {
            let epoch: i64 = active_epoch
                .number
                .try_into()
                .map_err(|_| NodeError::protocol("backup content epoch exceeds storage range"))?;
            transaction
                .execute(
                    "INSERT INTO content_keys (epoch, membership_head, key) VALUES (?1, ?2, ?3)",
                    turso::params![
                        epoch,
                        active_epoch.head.as_slice(),
                        active_epoch.key.as_slice()
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn is_recovered(&self) -> Result<bool, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query("SELECT recovered FROM community WHERE singleton = 1", ())
            .await?;
        let recovered: i64 = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("community is not initialized"))?
            .get(0)?;
        Ok(recovered == 1)
    }

    pub async fn initialize_community(
        &self,
        configured_community: Option<(CommunityId, MemberId)>,
        local_member: MemberId,
    ) -> Result<MemberId, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query("SELECT owner, id FROM community WHERE singleton = 1", ())
            .await?;
        if let Some(row) = rows.next().await? {
            let owner = member_from_row(row.get(0)?, "stored community owner")?;
            let stored_id: Option<Vec<u8>> = row.get(1)?;
            let id = if let Some(id) = stored_id {
                CommunityId::from_bytes(
                    id.try_into()
                        .map_err(|_| NodeError::protocol("stored community id is not 32 bytes"))?,
                )
            } else {
                let id = CommunityId::legacy(owner);
                connection
                    .execute(
                        "UPDATE community SET id = ?1 WHERE singleton = 1",
                        [id.as_bytes().as_slice()],
                    )
                    .await?;
                id
            };
            if configured_community.is_some_and(|configured| configured != (id, owner)) {
                return Err(NodeError::authorization(
                    "configured identity does not match the stored community",
                ));
            }
            return Ok(owner);
        }

        let (id, owner) =
            configured_community.unwrap_or_else(|| (CommunityId::generate(), local_member));
        connection
            .execute(
                "INSERT INTO community (singleton, owner, id) VALUES (1, ?1, ?2)",
                turso::params![owner.as_bytes().as_slice(), id.as_bytes().as_slice()],
            )
            .await?;
        Ok(owner)
    }

    pub async fn community_id(&self) -> Result<CommunityId, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query("SELECT id FROM community WHERE singleton = 1", ())
            .await?;
        let id: Vec<u8> = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("community is not initialized"))?
            .get(0)?;
        id.try_into()
            .map(CommunityId::from_bytes)
            .map_err(|_| NodeError::protocol("stored community id is not 32 bytes"))
    }

    pub async fn community_owner(&self) -> Result<MemberId, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query("SELECT owner FROM community WHERE singleton = 1", ())
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("community is not initialized"))?;
        member_from_row(row.get(0)?, "stored community owner")
    }

    pub async fn community(&self) -> Result<Community, NodeError> {
        let owner = self.community_owner().await?;
        let mut community = Community::new(owner);
        for signed in self.membership_history().await? {
            community
                .change_membership(owner, signed.update().change())
                .map_err(NodeError::authorization)?;
        }
        Ok(community)
    }

    pub async fn next_membership_revision(&self) -> Result<u64, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT COALESCE(MAX(revision), 0) FROM membership_changes",
                (),
            )
            .await?;
        let revision: i64 = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("membership revision query returned no row"))?
            .get(0)?;
        u64::try_from(revision)
            .ok()
            .and_then(|revision| revision.checked_add(1))
            .ok_or_else(|| NodeError::protocol("membership revision overflow"))
    }

    pub async fn insert_membership(&self, signed: &SignedMembership) -> Result<bool, NodeError> {
        let _guard = self.membership_mutation.lock().await;
        let update = signed.update();
        let owner = self.community_owner().await?;
        let mut community = self.community().await?;
        community
            .change_membership(owner, update.change())
            .map_err(NodeError::authorization)?;
        let revision: i64 = update
            .revision()
            .try_into()
            .map_err(|_| NodeError::protocol("membership revision exceeds storage range"))?;
        let connection = self.database.connect()?;
        let member = update.change().member();
        let member = member.as_bytes().as_slice();
        let admitted = i64::from(update.change().is_admission());
        let availability = i64::from(update.change().is_availability());
        let signature = signed.signature().as_slice();
        let conflict_key = signed.conflict_key();
        let changed = connection
            .execute(
                "INSERT INTO membership_changes \
                    (revision, member, admitted, availability, signature, conflict_key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(revision) DO NOTHING",
                turso::params![
                    revision,
                    member,
                    admitted,
                    availability,
                    signature,
                    conflict_key.as_slice(),
                ],
            )
            .await?;
        if changed == 1 {
            return Ok(true);
        }
        let mut rows = connection.query(
            "SELECT member, admitted, availability, signature, conflict_key FROM membership_changes WHERE revision = ?1",
            [revision],
        ).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("membership conflict disappeared"))?;
        let stored_member: Vec<u8> = row.get(0)?;
        let stored_admitted: i64 = row.get(1)?;
        let stored_availability: i64 = row.get(2)?;
        let stored_signature: Vec<u8> = row.get(3)?;
        let stored_conflict: Vec<u8> = row.get(4)?;
        if stored_member.as_slice() == member
            && stored_admitted == admitted
            && stored_availability == availability
            && stored_signature.as_slice() == signature
            && stored_conflict.as_slice() == conflict_key
        {
            Ok(false)
        } else {
            Err(NodeError::protocol(
                "owner equivocated at a membership revision",
            ))
        }
    }

    pub async fn persist_rotation(
        &self,
        signed: &SignedMembership,
        epoch: ContentEpoch,
        envelopes: &[ContentKeyEnvelope],
    ) -> Result<(), NodeError> {
        let _guard = self.membership_mutation.lock().await;
        let update = signed.update();
        let revision: i64 = update
            .revision()
            .try_into()
            .map_err(|_| NodeError::protocol("membership revision exceeds storage range"))?;
        let epoch_number: i64 = epoch
            .number
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        if epoch.number != update.revision()
            || epoch.head != signed.head(self.community_id().await?)
        {
            return Err(NodeError::protocol(
                "content epoch is not bound to the membership operation",
            ));
        }
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        let member = update.change().member();
        let conflict_key = signed.conflict_key();
        let changed = transaction.execute(
            "INSERT INTO membership_changes (revision, member, admitted, availability, signature, conflict_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(revision) DO NOTHING",
            turso::params![revision, member.as_bytes().as_slice(), i64::from(update.change().is_admission()), i64::from(update.change().is_availability()), signed.signature().as_slice(), conflict_key.as_slice()],
        ).await?;
        if changed != 1 {
            return Err(NodeError::protocol("membership revision already exists"));
        }
        transaction
            .execute(
                "INSERT INTO content_keys (epoch, membership_head, key) VALUES (?1, ?2, ?3)",
                turso::params![epoch_number, epoch.head.as_slice(), epoch.key.as_slice()],
            )
            .await?;
        for envelope in envelopes {
            transaction.execute(
                "INSERT INTO content_key_envelopes (epoch, membership_head, recipient, encapsulated_key, ciphertext, signature) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                turso::params![epoch_number, envelope.head.as_slice(), envelope.recipient.as_bytes().as_slice(), envelope.encapsulated_key.as_slice(), envelope.ciphertext.as_slice(), envelope.signature.as_slice()],
            ).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Snapshot, NodeError> {
        let community = self.community().await?;
        let mut messages: Vec<AuthoredText> = self
            .signed_messages(None)
            .await?
            .into_iter()
            .map(|signed| signed.authored().clone())
            .collect();
        for encrypted in self.encrypted_messages().await? {
            if let Some(key) = self.content_key(encrypted.epoch, &encrypted.head).await?
                && let Ok(message) = encrypted.decrypt(self.community_id().await?, &key)
            {
                messages.push(message);
            }
        }
        messages.sort_by_key(|authored| (authored.message().id(), authored.author()));
        let mut attachments: Vec<AuthoredAttachment> = Vec::new();
        for encrypted in self.encrypted_attachments().await? {
            if let Some(key) = self.content_key(encrypted.epoch, &encrypted.head).await?
                && let Ok(attachment) = encrypted.decrypt(self.community_id().await?, &key)
            {
                attachments.push(attachment);
            }
        }
        attachments.sort_by_key(|authored| (authored.attachment().id(), authored.author()));
        let channels = self
            .signed_channels(None)
            .await?
            .into_iter()
            .map(|signed| signed.channel().clone())
            .collect();
        let mut display_names = BTreeMap::new();
        for profile in self.member_profiles().await? {
            if community.contains(profile.member)
                && let Some(key) = self.content_key(profile.epoch, &profile.head).await?
            {
                display_names.insert(
                    profile.member,
                    profile.decrypt(self.community_id().await?, &key)?,
                );
            }
        }
        Ok(Snapshot::new(
            channels,
            messages,
            attachments,
            community,
            display_names,
        ))
    }

    pub async fn insert_channel(&self, signed: &SignedCreateChannel) -> Result<bool, NodeError> {
        let _guard = self.channel_mutation.lock().await;
        if let Some(existing) = self.channel(signed.channel().id()).await? {
            if existing == *signed.channel() {
                return Ok(false);
            }
            return Err(NodeError::protocol("channel id already exists"));
        }
        let channel = signed.channel();
        self.database
            .connect()?
            .execute(
                "INSERT INTO channels (id, name, kind, signature) VALUES (?1, ?2, ?3, ?4)",
                turso::params![
                    channel.id().as_bytes().as_slice(),
                    channel.name(),
                    channel_kind(channel.kind()),
                    signed.signature().as_slice(),
                ],
            )
            .await?;
        Ok(true)
    }

    pub async fn channel(&self, id: ChannelId) -> Result<Option<Channel>, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT name, kind FROM channels WHERE id = ?1",
                [id.as_bytes().as_slice()],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| channel_from_row(id, row.get(0)?, row.get(1)?))
            .transpose()
    }

    pub async fn last_read(&self, channel: ChannelId) -> Result<Option<MessageId>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT message_id FROM local_read_state WHERE channel_id = ?1",
                [channel.as_bytes().as_slice()],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| {
                row.get::<Vec<u8>>(0)?
                    .try_into()
                    .map(MessageId::from_bytes)
                    .map_err(|_| NodeError::protocol("stored read cursor is not 32 bytes"))
            })
            .transpose()
    }

    pub async fn set_last_read(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<(), NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT 1 FROM (\
                     SELECT channel_id, id FROM messages \
                     WHERE author IS NOT NULL AND signature IS NOT NULL AND ciphertext IS NULL \
                     UNION ALL SELECT channel_id, id FROM encrypted_messages \
                     UNION ALL SELECT channel_id, id FROM encrypted_attachments AS attachment \
                     WHERE NOT EXISTS (SELECT 1 FROM forgotten_attachments AS forgotten \
                         WHERE forgotten.author = attachment.author AND forgotten.id = attachment.id)\
                 ) WHERE channel_id = ?1 AND id = ?2 LIMIT 1",
                turso::params![channel.as_bytes().as_slice(), message.as_bytes().as_slice()],
            )
            .await?;
        if rows.next().await?.is_none() {
            return Err(NodeError::protocol(
                "read cursor does not identify a stored item in the channel",
            ));
        }
        drop(rows);
        connection
            .execute(
                "INSERT INTO local_read_state (channel_id, message_id) VALUES (?1, ?2) \
                 ON CONFLICT(channel_id) DO UPDATE SET message_id = excluded.message_id \
                 WHERE excluded.message_id > local_read_state.message_id",
                turso::params![channel.as_bytes().as_slice(), message.as_bytes().as_slice()],
            )
            .await?;
        Ok(())
    }

    pub async fn sync_state(
        &self,
    ) -> Result<
        (
            Vec<SignedMembership>,
            Vec<KeyRegistration>,
            Vec<ContentKeyEnvelope>,
            Vec<SignedCreateChannel>,
            Vec<EncryptedText>,
            Vec<EncryptedAttachment>,
            Vec<EncryptedMemberProfile>,
        ),
        NodeError,
    > {
        let memberships = self.membership_history().await?;
        let registrations = self.key_registrations().await?;
        let envelopes = self.content_key_envelopes().await?;
        let channels = self.signed_channels(None).await?;
        let messages = self.encrypted_messages().await?;
        let attachments = self.encrypted_attachments().await?;
        let community = self.community().await?;
        let (active_epoch, active_head) = self.active_membership_head().await?;
        let member_profiles = self
            .member_profiles()
            .await?
            .into_iter()
            .filter(|profile| {
                community.contains(profile.member)
                    && profile.epoch == active_epoch
                    && profile.head == active_head
            })
            .collect();
        Ok((
            memberships,
            registrations,
            envelopes,
            channels,
            messages,
            attachments,
            member_profiles,
        ))
    }

    pub async fn next_member_profile_revision(&self, member: MemberId) -> Result<u64, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT COALESCE(MAX(revision), 0) FROM member_profiles WHERE member = ?1",
                [member.as_bytes().as_slice()],
            )
            .await?;
        let revision: i64 = rows
            .next()
            .await?
            .ok_or_else(|| NodeError::protocol("member profile revision query returned no row"))?
            .get(0)?;
        u64::try_from(revision)
            .ok()
            .and_then(|revision| revision.checked_add(1))
            .ok_or_else(|| NodeError::protocol("member profile revision overflow"))
    }

    pub async fn insert_member_profile(
        &self,
        profile: &EncryptedMemberProfile,
    ) -> Result<bool, NodeError> {
        let _guard = self.member_profile_mutation.lock().await;
        let existing = self.member_profile(profile.member).await?;
        if let Some(existing) = existing {
            if existing.revision > profile.revision {
                return Ok(false);
            }
            if existing.revision == profile.revision {
                if existing.epoch == profile.epoch
                    && existing.head == profile.head
                    && existing.nonce == profile.nonce
                    && existing.ciphertext == profile.ciphertext
                    && existing.signature == profile.signature
                {
                    return Ok(false);
                }
                return Err(NodeError::protocol(
                    "member equivocated at a profile revision",
                ));
            }
        }
        let revision: i64 = profile
            .revision
            .try_into()
            .map_err(|_| NodeError::protocol("member profile revision exceeds storage range"))?;
        let epoch: i64 = profile
            .epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        self.database.connect()?.execute(
            "INSERT INTO member_profiles (member, revision, epoch, membership_head, nonce, ciphertext, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(member) DO UPDATE SET \
             revision = excluded.revision, epoch = excluded.epoch, membership_head = excluded.membership_head, \
             nonce = excluded.nonce, ciphertext = excluded.ciphertext, signature = excluded.signature",
            turso::params![profile.member.as_bytes().as_slice(), revision, epoch, profile.head.as_slice(), profile.nonce.as_slice(), profile.ciphertext.as_slice(), profile.signature.as_slice()],
        ).await?;
        Ok(true)
    }

    pub(crate) async fn member_profile(
        &self,
        member: MemberId,
    ) -> Result<Option<EncryptedMemberProfile>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT revision, epoch, membership_head, nonce, ciphertext, signature FROM member_profiles WHERE member = ?1",
            [member.as_bytes().as_slice()],
        ).await?;
        rows.next()
            .await?
            .map(|row| {
                member_profile_from_row(
                    member,
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                )
            })
            .transpose()
    }

    async fn member_profiles(&self) -> Result<Vec<EncryptedMemberProfile>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT member, revision, epoch, membership_head, nonce, ciphertext, signature FROM member_profiles ORDER BY member",
            (),
        ).await?;
        let mut profiles = Vec::new();
        while let Some(row) = rows.next().await? {
            profiles.push(member_profile_from_row(
                member_from_row(row.get(0)?, "stored member profile member")?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            )?);
        }
        Ok(profiles)
    }

    pub async fn insert_key_registration(
        &self,
        registration: &KeyRegistration,
    ) -> Result<bool, NodeError> {
        let changed = self
            .database
            .connect()?
            .execute(
                "INSERT INTO key_registrations (member, public_key, signature) VALUES (?1, ?2, ?3) \
             ON CONFLICT(member) DO NOTHING",
                turso::params![
                    registration.member().as_bytes().as_slice(),
                    registration.public_key().as_slice(),
                    registration.signature().as_slice()
                ],
            )
            .await?;
        if changed == 0 {
            let existing = self
                .key_registration(registration.member())
                .await?
                .ok_or_else(|| NodeError::protocol("key registration disappeared"))?;
            if existing.public_key() != registration.public_key()
                || existing.signature() != registration.signature()
            {
                return Err(NodeError::protocol(
                    "member already registered a different encryption key",
                ));
            }
        }
        Ok(changed == 1)
    }

    pub async fn key_registration(
        &self,
        member: MemberId,
    ) -> Result<Option<KeyRegistration>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT public_key, signature FROM key_registrations WHERE member = ?1",
                [member.as_bytes().as_slice()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let public_key: [u8; 32] = row
            .get::<Vec<u8>>(0)?
            .try_into()
            .map_err(|_| NodeError::protocol("stored HPKE public key is not 32 bytes"))?;
        let signature: [u8; 64] = row.get::<Vec<u8>>(1)?.try_into().map_err(|_| {
            NodeError::protocol("stored key registration signature is not 64 bytes")
        })?;
        Ok(Some(KeyRegistration::verified(
            self.community_id().await?,
            member,
            public_key,
            signature,
        )?))
    }

    async fn key_registrations(&self) -> Result<Vec<KeyRegistration>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT member, public_key, signature FROM key_registrations ORDER BY member",
                (),
            )
            .await?;
        let mut result = Vec::new();
        let id = self.community_id().await?;
        while let Some(row) = rows.next().await? {
            let member = member_from_row(row.get(0)?, "stored key registration member")?;
            let public_key = row
                .get::<Vec<u8>>(1)?
                .try_into()
                .map_err(|_| NodeError::protocol("stored HPKE public key is not 32 bytes"))?;
            let signature = row.get::<Vec<u8>>(2)?.try_into().map_err(|_| {
                NodeError::protocol("stored key registration signature is not 64 bytes")
            })?;
            result.push(KeyRegistration::verified(
                id, member, public_key, signature,
            )?);
        }
        Ok(result)
    }

    pub async fn insert_content_key(&self, epoch: ContentEpoch) -> Result<(), NodeError> {
        let epoch_number: i64 = epoch
            .number
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let connection = self.database.connect()?;
        let changed = connection
            .execute(
                "INSERT INTO content_keys (epoch, membership_head, key) VALUES (?1, ?2, ?3) \
             ON CONFLICT(epoch, membership_head) DO NOTHING",
                turso::params![epoch_number, epoch.head.as_slice(), epoch.key.as_slice()],
            )
            .await?;
        if changed == 0 {
            let existing = self
                .content_key(epoch.number, &epoch.head)
                .await?
                .ok_or_else(|| NodeError::protocol("content key conflict disappeared"))?;
            if existing != epoch.key {
                return Err(NodeError::protocol(
                    "content epoch identifies a different key",
                ));
            }
        }
        Ok(())
    }

    pub async fn active_membership_head(&self) -> Result<(u64, [u8; 32]), NodeError> {
        if let Some(membership) = self.membership_history().await?.last() {
            return Ok((
                membership.update().revision(),
                membership.head(self.community_id().await?),
            ));
        }
        let owner = self.community_owner().await?;
        Ok((0, genesis_head(self.community_id().await?, owner)))
    }

    pub async fn authorize_epoch_author(
        &self,
        epoch: u64,
        head: &[u8; 32],
        author: MemberId,
    ) -> Result<(), NodeError> {
        let owner = self.community_owner().await?;
        let community_id = self.community_id().await?;
        if epoch == 0 {
            if *head != genesis_head(community_id, owner) {
                return Err(NodeError::protocol(
                    "content references an unknown genesis head",
                ));
            }
            return Community::new(owner)
                .authorize_participant(author)
                .map_err(NodeError::authorization);
        }

        let mut community = Community::new(owner);
        for membership in self.membership_history().await? {
            community
                .change_membership(owner, membership.update().change())
                .map_err(NodeError::authorization)?;
            if membership.update().revision() == epoch {
                if membership.head(community_id) != *head {
                    return Err(NodeError::protocol(
                        "content references a non-canonical membership head",
                    ));
                }
                return community
                    .authorize_participant(author)
                    .map_err(NodeError::authorization);
            }
        }
        Err(NodeError::protocol(
            "content references an unknown membership epoch",
        ))
    }

    pub async fn active_content_epoch(&self) -> Result<Option<ContentEpoch>, NodeError> {
        let (epoch, head) = self.active_membership_head().await?;
        let epoch: i64 = epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT epoch, membership_head, key FROM content_keys WHERE epoch = ?1 AND membership_head = ?2 LIMIT 1",
                turso::params![epoch, head.as_slice()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(epoch_from_row(row.get(0)?, row.get(1)?, row.get(2)?)?))
    }

    pub(crate) async fn has_content_keys(&self) -> Result<bool, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query("SELECT 1 FROM content_keys LIMIT 1", ())
            .await?;
        Ok(rows.next().await?.is_some())
    }

    async fn latest_content_epoch(&self) -> Result<Option<ContentEpoch>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT epoch, membership_head, key FROM content_keys \
                 ORDER BY epoch DESC, membership_head LIMIT 1",
                (),
            )
            .await?;
        rows.next()
            .await?
            .map(|row| epoch_from_row(row.get(0)?, row.get(1)?, row.get(2)?))
            .transpose()
    }

    pub(crate) async fn content_key(
        &self,
        epoch: u64,
        head: &[u8; 32],
    ) -> Result<Option<[u8; 32]>, NodeError> {
        let epoch: i64 = epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT key FROM content_keys WHERE epoch = ?1 AND membership_head = ?2",
                turso::params![epoch, head.as_slice()],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| {
                row.get::<Vec<u8>>(0)?
                    .try_into()
                    .map_err(|_| NodeError::protocol("stored content key is not 32 bytes"))
            })
            .transpose()
    }

    pub async fn insert_content_key_envelope(
        &self,
        envelope: &ContentKeyEnvelope,
    ) -> Result<bool, NodeError> {
        let epoch: i64 = envelope
            .epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let changed = self.database.connect()?.execute(
            "INSERT INTO content_key_envelopes (epoch, membership_head, recipient, encapsulated_key, ciphertext, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(epoch, membership_head, recipient) DO NOTHING",
            turso::params![epoch, envelope.head.as_slice(), envelope.recipient.as_bytes().as_slice(), envelope.encapsulated_key.as_slice(), envelope.ciphertext.as_slice(), envelope.signature.as_slice()],
        ).await?;
        if changed == 0 {
            let existing = self
                .content_key_envelope(envelope.epoch, &envelope.head, envelope.recipient)
                .await?
                .ok_or_else(|| NodeError::protocol("content key envelope conflict disappeared"))?;
            if existing.encapsulated_key != envelope.encapsulated_key
                || existing.ciphertext != envelope.ciphertext
                || existing.signature != envelope.signature
            {
                return Err(NodeError::protocol(
                    "content epoch identifies a different recipient envelope",
                ));
            }
        }
        Ok(changed == 1)
    }

    pub async fn content_key_envelope(
        &self,
        epoch: u64,
        head: &[u8; 32],
        recipient: MemberId,
    ) -> Result<Option<ContentKeyEnvelope>, NodeError> {
        let epoch: i64 = epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let mut rows = self.database.connect()?.query(
            "SELECT epoch, membership_head, recipient, encapsulated_key, ciphertext, signature FROM content_key_envelopes WHERE epoch = ?1 AND membership_head = ?2 AND recipient = ?3",
            turso::params![epoch, head.as_slice(), recipient.as_bytes().as_slice()],
        ).await?;
        rows.next()
            .await?
            .map(|row| {
                envelope_from_row(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                )
            })
            .transpose()
    }

    async fn content_key_envelopes(&self) -> Result<Vec<ContentKeyEnvelope>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT epoch, membership_head, recipient, encapsulated_key, ciphertext, signature FROM content_key_envelopes ORDER BY epoch, recipient", (),
        ).await?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().await? {
            result.push(envelope_from_row(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            )?);
        }
        Ok(result)
    }

    pub async fn insert_encrypted(&self, message: &EncryptedText) -> Result<bool, NodeError> {
        let epoch: i64 = message
            .epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let changed = self.database.connect()?.execute(
            "INSERT INTO encrypted_messages (id, author, signature, channel_id, epoch, membership_head, nonce, ciphertext) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(author, id) DO NOTHING",
            turso::params![message.id.as_bytes().as_slice(), message.author.as_bytes().as_slice(), message.signature.as_slice(), message.channel_id.as_bytes().as_slice(), epoch, message.head.as_slice(), message.nonce.as_slice(), message.ciphertext.as_slice()],
        ).await?;
        if changed == 0 {
            let existing = self.encrypted_message(message.author, message.id).await?;
            if existing.as_ref().is_none_or(|existing| {
                existing.author != message.author
                    || existing.channel_id != message.channel_id
                    || existing.epoch != message.epoch
                    || existing.head != message.head
                    || existing.nonce != message.nonce
                    || existing.ciphertext != message.ciphertext
                    || existing.signature != message.signature
            }) {
                return Err(NodeError::protocol(
                    "message id already identifies a different encrypted operation",
                ));
            }
        }
        Ok(changed == 1)
    }

    async fn encrypted_message(
        &self,
        author: MemberId,
        id: MessageId,
    ) -> Result<Option<EncryptedText>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT channel_id, author, epoch, membership_head, nonce, ciphertext, signature \
             FROM encrypted_messages WHERE author = ?1 AND id = ?2",
                turso::params![author.as_bytes().as_slice(), id.as_bytes().as_slice()],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| {
                encrypted_from_row(
                    id,
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                )
            })
            .transpose()
    }

    async fn encrypted_messages(&self) -> Result<Vec<EncryptedText>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT id, channel_id, author, epoch, membership_head, nonce, ciphertext, signature \
             FROM encrypted_messages ORDER BY id, author", (),
        ).await?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = row
                .get::<Vec<u8>>(0)?
                .try_into()
                .map(MessageId::from_bytes)
                .map_err(|_| NodeError::protocol("stored message id is not 32 bytes"))?;
            result.push(encrypted_from_row(
                id,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            )?);
        }
        Ok(result)
    }

    pub(crate) async fn encrypted_text_inventory(
        &self,
    ) -> Result<BTreeSet<(MemberId, MessageId)>, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query("SELECT author, id FROM encrypted_messages", ())
            .await?;
        let mut inventory = BTreeSet::new();
        while let Some(row) = rows.next().await? {
            let author = member_from_row(row.get(0)?, "stored encrypted message author")?;
            let id = row
                .get::<Vec<u8>>(1)?
                .try_into()
                .map(MessageId::from_bytes)
                .map_err(|_| NodeError::protocol("stored message id is not 32 bytes"))?;
            inventory.insert((author, id));
        }
        Ok(inventory)
    }

    pub async fn insert_encrypted_attachment(
        &self,
        attachment: &EncryptedAttachment,
    ) -> Result<bool, NodeError> {
        if self
            .attachment_is_forgotten(attachment.author, attachment.id)
            .await?
        {
            return Ok(false);
        }
        let epoch: i64 = attachment
            .epoch
            .try_into()
            .map_err(|_| NodeError::protocol("content epoch exceeds storage range"))?;
        let changed = self.database.connect()?.execute(
            "INSERT INTO encrypted_attachments (id, author, channel_id, epoch, membership_head, nonce, ciphertext, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(author, id) DO NOTHING",
            turso::params![attachment.id.as_bytes().as_slice(), attachment.author.as_bytes().as_slice(), attachment.channel_id.as_bytes().as_slice(), epoch, attachment.head.as_slice(), attachment.nonce.as_slice(), attachment.ciphertext.as_slice(), attachment.signature.as_slice()],
        ).await?;
        if changed == 0 {
            let existing = self
                .encrypted_attachment(attachment.author, attachment.id)
                .await?;
            if existing.as_ref().is_none_or(|existing| {
                existing.channel_id != attachment.channel_id
                    || existing.epoch != attachment.epoch
                    || existing.head != attachment.head
                    || existing.nonce != attachment.nonce
                    || existing.ciphertext != attachment.ciphertext
                    || existing.signature != attachment.signature
            }) {
                return Err(NodeError::protocol(
                    "attachment id already identifies a different operation",
                ));
            }
        }
        Ok(changed == 1)
    }

    pub async fn forget_attachment(
        &self,
        author: MemberId,
        id: MessageId,
    ) -> Result<(), NodeError> {
        let mut connection = self.database.connect()?;
        let transaction = connection.transaction().await?;
        transaction.execute(
            "INSERT INTO forgotten_attachments (author, id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            turso::params![author.as_bytes().as_slice(), id.as_bytes().as_slice()],
        ).await?;
        transaction
            .execute(
                "DELETE FROM encrypted_attachments WHERE author = ?1 AND id = ?2",
                turso::params![author.as_bytes().as_slice(), id.as_bytes().as_slice()],
            )
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn attachment_is_forgotten(
        &self,
        author: MemberId,
        id: MessageId,
    ) -> Result<bool, NodeError> {
        let mut rows = self
            .database
            .connect()?
            .query(
                "SELECT 1 FROM forgotten_attachments WHERE author = ?1 AND id = ?2",
                turso::params![author.as_bytes().as_slice(), id.as_bytes().as_slice()],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    async fn encrypted_attachment(
        &self,
        author: MemberId,
        id: MessageId,
    ) -> Result<Option<EncryptedAttachment>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT channel_id, author, epoch, membership_head, nonce, ciphertext, signature FROM encrypted_attachments WHERE author = ?1 AND id = ?2",
            turso::params![author.as_bytes().as_slice(), id.as_bytes().as_slice()],
        ).await?;
        rows.next()
            .await?
            .map(|row| {
                encrypted_attachment_from_row(
                    id,
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                )
            })
            .transpose()
    }

    async fn encrypted_attachments(&self) -> Result<Vec<EncryptedAttachment>, NodeError> {
        let mut rows = self.database.connect()?.query(
            "SELECT id, channel_id, author, epoch, membership_head, nonce, ciphertext, signature FROM encrypted_attachments ORDER BY id, author", (),
        ).await?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = row
                .get::<Vec<u8>>(0)?
                .try_into()
                .map(MessageId::from_bytes)
                .map_err(|_| NodeError::protocol("stored attachment id is not 32 bytes"))?;
            result.push(encrypted_attachment_from_row(
                id,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            )?);
        }
        Ok(result)
    }

    async fn membership_history(&self) -> Result<Vec<SignedMembership>, NodeError> {
        let connection = self.database.connect()?;
        let mut rows = connection
            .query(
                "SELECT revision, member, admitted, availability, signature, conflict_key \
                 FROM membership_changes ORDER BY revision, conflict_key",
                (),
            )
            .await?;
        let owner = self.community_owner().await?;
        let mut memberships = Vec::new();
        while let Some(row) = rows.next().await? {
            let revision: i64 = row.get(0)?;
            let member = member_from_row(row.get(1)?, "stored membership member")?;
            let admitted: i64 = row.get(2)?;
            let availability: i64 = row.get(3)?;
            let signature: Vec<u8> = row.get(4)?;
            let conflict_key: Vec<u8> = row.get(5)?;
            let signature = signature
                .try_into()
                .map_err(|_| NodeError::protocol("stored membership signature is not 64 bytes"))?;
            let change = match (admitted, availability) {
                (1, 1) => MembershipChange::AdmitAvailability(member),
                (1, 0) => MembershipChange::Admit(member),
                (0, 0) => MembershipChange::Remove(member),
                _ => return Err(NodeError::protocol("stored membership role is invalid")),
            };
            let update = MembershipUpdate::new(
                revision
                    .try_into()
                    .map_err(|_| NodeError::protocol("stored membership revision is invalid"))?,
                change,
            );
            let signed =
                SignedMembership::verified(owner, self.community_id().await?, update, signature)
                    .map_err(NodeError::protocol)?;
            if conflict_key.as_slice() != signed.conflict_key() {
                return Err(NodeError::protocol(
                    "stored membership conflict key is invalid",
                ));
            }
            memberships.push(signed);
        }
        Ok(memberships)
    }

    async fn signed_messages(&self, limit: Option<usize>) -> Result<Vec<SignedText>, NodeError> {
        let connection = self.database.connect()?;
        let query = "SELECT id, body, author, signature, channel_id FROM messages \
                     WHERE author IS NOT NULL AND signature IS NOT NULL AND ciphertext IS NULL ORDER BY id";
        let mut rows = if let Some(limit) = limit {
            connection
                .query(&format!("{query} LIMIT {limit}"), ())
                .await?
        } else {
            connection.query(query, ()).await?
        };
        let mut messages = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: Vec<u8> = row.get(0)?;
            let body: String = row.get(1)?;
            let author: Vec<u8> = row.get(2)?;
            let signature: Vec<u8> = row.get(3)?;
            let channel_id: Vec<u8> = row.get(4)?;
            let id = id
                .try_into()
                .map_err(|_| NodeError::protocol("stored message id is not 32 bytes"))?;
            let author = author
                .try_into()
                .map_err(|_| NodeError::protocol("stored author id is not 32 bytes"))?;
            let channel_id = channel_id
                .try_into()
                .map_err(|_| NodeError::protocol("stored channel id is not 32 bytes"))?;
            let message = TextMessage::in_channel(
                MessageId::from_bytes(id),
                ChannelId::from_bytes(channel_id),
                body,
            )
            .map_err(NodeError::from)?;
            let signature = signature
                .try_into()
                .map_err(|_| NodeError::protocol("stored signature is not 64 bytes"))?;
            messages.push(
                SignedText::verified(
                    AuthoredText::new(MemberId::from_bytes(author), message),
                    signature,
                )
                .map_err(NodeError::protocol)?,
            );
        }
        Ok(messages)
    }

    async fn signed_channels(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<SignedCreateChannel>, NodeError> {
        let connection = self.database.connect()?;
        let query = "SELECT id, name, kind, signature FROM channels ORDER BY id";
        let mut rows = if let Some(limit) = limit {
            connection
                .query(&format!("{query} LIMIT {limit}"), ())
                .await?
        } else {
            connection.query(query, ()).await?
        };
        let owner = self.community_owner().await?;
        let community_id = self.community_id().await?;
        let mut channels = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: Vec<u8> = row.get(0)?;
            let id = id
                .try_into()
                .map(ChannelId::from_bytes)
                .map_err(|_| NodeError::protocol("stored channel id is not 32 bytes"))?;
            let channel = channel_from_row(id, row.get(1)?, row.get(2)?)?;
            let signature: Vec<u8> = row.get(3)?;
            let signature = signature
                .try_into()
                .map_err(|_| NodeError::protocol("stored channel signature is not 64 bytes"))?;
            channels.push(
                SignedCreateChannel::verified(owner, community_id, channel, signature)
                    .map_err(NodeError::protocol)?,
            );
        }
        Ok(channels)
    }
}

fn channel_kind(kind: ChannelKind) -> i64 {
    i64::from(kind.number())
}

fn channel_from_row(id: ChannelId, name: String, kind: i64) -> Result<Channel, NodeError> {
    let kind = ChannelKind::from_number(kind)
        .ok_or_else(|| NodeError::protocol("stored channel kind is invalid"))?;
    Channel::new(id, name, kind).map_err(NodeError::from)
}

fn member_from_row(bytes: Vec<u8>, label: &str) -> Result<MemberId, NodeError> {
    bytes
        .try_into()
        .map(MemberId::from_bytes)
        .map_err(|_| NodeError::protocol(format_args!("{label} is not 32 bytes")))
}

fn epoch_from_row(epoch: i64, head: Vec<u8>, key: Vec<u8>) -> Result<ContentEpoch, NodeError> {
    Ok(ContentEpoch {
        number: epoch
            .try_into()
            .map_err(|_| NodeError::protocol("stored content epoch is invalid"))?,
        head: head
            .try_into()
            .map_err(|_| NodeError::protocol("stored membership head is not 32 bytes"))?,
        key: key
            .try_into()
            .map_err(|_| NodeError::protocol("stored content key is not 32 bytes"))?,
    })
}

fn envelope_from_row(
    epoch: i64,
    head: Vec<u8>,
    recipient: Vec<u8>,
    encapsulated_key: Vec<u8>,
    ciphertext: Vec<u8>,
    signature: Vec<u8>,
) -> Result<ContentKeyEnvelope, NodeError> {
    Ok(ContentKeyEnvelope {
        epoch: epoch
            .try_into()
            .map_err(|_| NodeError::protocol("stored content epoch is invalid"))?,
        head: head
            .try_into()
            .map_err(|_| NodeError::protocol("stored membership head is not 32 bytes"))?,
        recipient: member_from_row(recipient, "stored envelope recipient")?,
        encapsulated_key: encapsulated_key
            .try_into()
            .map_err(|_| NodeError::protocol("stored HPKE encapsulated key is not 32 bytes"))?,
        ciphertext,
        signature: signature
            .try_into()
            .map_err(|_| NodeError::protocol("stored envelope signature is not 64 bytes"))?,
    })
}

#[allow(clippy::too_many_arguments)]
fn encrypted_from_row(
    id: MessageId,
    channel: Vec<u8>,
    author: Vec<u8>,
    epoch: i64,
    head: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    signature: Vec<u8>,
) -> Result<EncryptedText, NodeError> {
    Ok(EncryptedText {
        id,
        channel_id: ChannelId::from_bytes(
            channel
                .try_into()
                .map_err(|_| NodeError::protocol("stored channel id is not 32 bytes"))?,
        ),
        author: member_from_row(author, "stored encrypted message author")?,
        epoch: epoch
            .try_into()
            .map_err(|_| NodeError::protocol("stored content epoch is invalid"))?,
        head: head
            .try_into()
            .map_err(|_| NodeError::protocol("stored membership head is not 32 bytes"))?,
        nonce: nonce
            .try_into()
            .map_err(|_| NodeError::protocol("stored text nonce is not 24 bytes"))?,
        ciphertext,
        signature: signature
            .try_into()
            .map_err(|_| NodeError::protocol("stored text signature is not 64 bytes"))?,
    })
}

fn member_profile_from_row(
    member: MemberId,
    revision: i64,
    epoch: i64,
    head: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    signature: Vec<u8>,
) -> Result<EncryptedMemberProfile, NodeError> {
    Ok(EncryptedMemberProfile {
        member,
        revision: revision
            .try_into()
            .map_err(|_| NodeError::protocol("stored member profile revision is invalid"))?,
        epoch: epoch
            .try_into()
            .map_err(|_| NodeError::protocol("stored member profile epoch is invalid"))?,
        head: head.try_into().map_err(|_| {
            NodeError::protocol("stored member profile membership head is not 32 bytes")
        })?,
        nonce: nonce
            .try_into()
            .map_err(|_| NodeError::protocol("stored member profile nonce is not 24 bytes"))?,
        ciphertext,
        signature: signature
            .try_into()
            .map_err(|_| NodeError::protocol("stored member profile signature is not 64 bytes"))?,
    })
}

#[allow(clippy::too_many_arguments)]
fn encrypted_attachment_from_row(
    id: MessageId,
    channel: Vec<u8>,
    author: Vec<u8>,
    epoch: i64,
    head: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    signature: Vec<u8>,
) -> Result<EncryptedAttachment, NodeError> {
    Ok(EncryptedAttachment {
        id,
        channel_id: ChannelId::from_bytes(
            channel
                .try_into()
                .map_err(|_| NodeError::protocol("stored attachment channel id is not 32 bytes"))?,
        ),
        author: member_from_row(author, "stored attachment author")?,
        epoch: epoch
            .try_into()
            .map_err(|_| NodeError::protocol("stored attachment epoch is invalid"))?,
        head: head.try_into().map_err(|_| {
            NodeError::protocol("stored attachment membership head is not 32 bytes")
        })?,
        nonce: nonce
            .try_into()
            .map_err(|_| NodeError::protocol("stored attachment nonce is not 24 bytes"))?,
        ciphertext,
        signature: signature
            .try_into()
            .map_err(|_| NodeError::protocol("stored attachment signature is not 64 bytes"))?,
    })
}

async fn ensure_table_column(
    connection: &turso::Connection,
    table: &str,
    name: &str,
    definition: &str,
) -> Result<(), NodeError> {
    let mut rows = connection
        .query(&format!("PRAGMA table_info({table})"), ())
        .await?;
    while let Some(row) = rows.next().await? {
        if row.get::<String>(1)? == name {
            return Ok(());
        }
    }
    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"),
            (),
        )
        .await?;
    Ok(())
}

async fn table_exists(connection: &turso::Connection, table: &str) -> Result<bool, NodeError> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    #[tokio::test]
    async fn rejects_owner_equivocation_at_one_revision() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        let owner_key = SigningKey::from_bytes(&[1; 32]);
        let owner = MemberId::from_bytes(owner_key.verifying_key().to_bytes());
        let id = CommunityId::from_bytes([2; 32]);
        store
            .initialize_community(Some((id, owner)), owner)
            .await
            .unwrap();
        let first = SignedMembership::sign(
            &owner_key,
            id,
            1,
            MembershipChange::Admit(MemberId::from_bytes([3; 32])),
        );
        let conflict = SignedMembership::sign(
            &owner_key,
            id,
            1,
            MembershipChange::Admit(MemberId::from_bytes([4; 32])),
        );
        assert!(store.insert_membership(&first).await.unwrap());
        assert!(store.insert_membership(&first).await.is_ok());
        assert_eq!(
            store.insert_membership(&conflict).await.unwrap_err().kind(),
            crate::FaultKind::Protocol
        );
    }

    #[tokio::test]
    async fn migrates_membership_roles_and_persists_availability() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("peer.db");
        let database = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .unwrap();
        let connection = database.connect().unwrap();
        connection
            .execute(
                "CREATE TABLE membership_changes (revision INTEGER PRIMARY KEY, member BLOB NOT NULL, admitted INTEGER NOT NULL, signature BLOB NOT NULL, conflict_key BLOB NOT NULL)",
                (),
            )
            .await
            .unwrap();
        connection
            .execute(
                "INSERT INTO membership_changes VALUES (1, ?1, 1, ?2, ?3)",
                turso::params![
                    [3_u8; 32].as_slice(),
                    [0_u8; 64].as_slice(),
                    [0_u8; 97].as_slice()
                ],
            )
            .await
            .unwrap();
        drop(connection);
        drop(database);

        let store = Store::open(directory.path()).await.unwrap();
        let mut rows = store
            .database
            .connect()
            .unwrap()
            .query(
                "SELECT availability FROM membership_changes WHERE revision = 1",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            0
        );
        drop(rows);

        let owner_key = SigningKey::from_bytes(&[1; 32]);
        let owner = MemberId::from_bytes(owner_key.verifying_key().to_bytes());
        let id = CommunityId::from_bytes([2; 32]);
        store
            .database
            .connect()
            .unwrap()
            .execute("DELETE FROM membership_changes", ())
            .await
            .unwrap();
        store
            .initialize_community(Some((id, owner)), owner)
            .await
            .unwrap();
        let availability = MemberId::from_bytes([4; 32]);
        store
            .insert_membership(&SignedMembership::sign(
                &owner_key,
                id,
                1,
                MembershipChange::AdmitAvailability(availability),
            ))
            .await
            .unwrap();
        assert_eq!(
            store.community().await.unwrap().role(availability),
            Some(crate::MemberRole::Availability)
        );
    }
}
