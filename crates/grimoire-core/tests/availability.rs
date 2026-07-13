use std::{path::Path, time::Duration};

use grimoire_core::{
    Attachment, Channel, ChannelId, ChannelKind, Command, Event, FaultKind, MemberRole,
    MembershipChange, MessageId, Node, NodeConfig, TextMessage,
};
use tokio::{sync::broadcast, time::timeout};

#[tokio::test]
async fn availability_peer_retains_and_serves_ciphertext_without_content_access() {
    let owner_dir = tempfile::tempdir().unwrap();
    let availability_dir = tempfile::tempdir().unwrap();
    let participant_dir = tempfile::tempdir().unwrap();
    let rotation_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let availability = Node::open(
        NodeConfig::new(availability_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let participant = Node::open(
        NodeConfig::new(participant_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();

    let availability_id = availability.member_id();
    let participant_id = participant.member_id();
    let mut availability_address = availability.address();
    let mut availability_events = availability.subscribe();
    connect_eventually(&owner, availability_address.clone()).await;
    owner
        .execute(Command::ChangeMembership(
            MembershipChange::AdmitAvailability(availability_id),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_membership(&mut availability_events)
            .await
            .role(availability_id),
        Some(MemberRole::Availability)
    );

    let mut participant_events = participant.subscribe();
    owner.connect(participant.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            participant.member_id(),
        )))
        .await
        .unwrap();
    next_membership(&mut participant_events).await;
    next_membership(&mut availability_events).await;
    participant.shutdown().await.unwrap();

    let rotation = Node::open(
        NodeConfig::new(rotation_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(rotation.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(
            MembershipChange::AdmitAvailability(rotation.member_id()),
        ))
        .await
        .unwrap();
    next_membership(&mut availability_events).await;
    rotation.shutdown().await.unwrap();

    owner
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: grimoire_core::VoicePresence::Joined,
        })
        .await
        .unwrap();
    availability.shutdown().await.unwrap();
    let availability = Node::open(NodeConfig::new(availability_dir.path()))
        .await
        .unwrap();
    availability_address = availability.address();
    let mut availability_events = availability.subscribe();
    connect_eventually(&owner, availability_address.clone()).await;

    assert_eq!(
        query_count(availability_dir.path(), "content_keys").await,
        0
    );
    assert_eq!(
        query_recipient_envelopes(availability_dir.path(), availability_id).await,
        0
    );
    assert!(query_recipient_envelopes(availability_dir.path(), participant_id).await > 0);
    assert_eq!(
        availability
            .execute(Command::PostText(
                TextMessage::new(MessageId::from_bytes([81; 32]), "forbidden").unwrap(),
            ))
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    assert_eq!(
        availability
            .execute(Command::CreateChannel(
                Channel::new(
                    ChannelId::from_bytes([81; 32]),
                    "forbidden",
                    ChannelKind::Text,
                )
                .unwrap(),
            ))
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    assert_eq!(
        availability
            .execute(Command::SetVoicePresence {
                channel: ChannelId::VOICE_ROOM,
                state: grimoire_core::VoicePresence::Joined,
            })
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );

    let message =
        TextMessage::new(MessageId::from_bytes([82; 32]), "stored but unreadable").unwrap();
    let attachment = Attachment::new(
        MessageId::from_bytes([83; 32]),
        ChannelId::GENERAL,
        "offline.txt",
        b"served by availability".to_vec(),
    )
    .unwrap();
    owner
        .execute(Command::PostText(message.clone()))
        .await
        .unwrap();
    owner
        .execute(Command::ShareAttachment(attachment.clone()))
        .await
        .unwrap();
    wait_for_count(availability_dir.path(), "encrypted_messages", 1).await;
    wait_for_count(availability_dir.path(), "encrypted_attachments", 1).await;
    let snapshot = availability.snapshot().await.unwrap();
    assert_eq!(
        snapshot.role(availability_id),
        Some(MemberRole::Availability)
    );
    assert!(snapshot.messages().is_empty());
    assert!(snapshot.attachments().is_empty());
    assert!(
        timeout(Duration::from_millis(200), async {
            loop {
                match availability_events.recv().await.unwrap() {
                    Event::TextStored(_) | Event::AttachmentStored(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .is_err()
    );

    owner.shutdown().await.unwrap();
    let participant = Node::open(NodeConfig::new(participant_dir.path()))
        .await
        .unwrap();
    let mut participant_events = participant.subscribe();
    connect_eventually(&participant, availability_address.clone()).await;
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = participant.snapshot().await.unwrap();
            if snapshot
                .messages()
                .iter()
                .any(|stored| stored.message() == &message)
                && snapshot
                    .attachments()
                    .iter()
                    .any(|stored| stored.attachment() == &attachment)
            {
                break;
            }
            tokio::select! {
                event = participant_events.recv() => {
                    if let Event::Fault(fault) = event.unwrap() {
                        panic!("availability catch-up failed: {fault}");
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    })
    .await
    .unwrap();

    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let mut availability_events = availability.subscribe();
    connect_eventually(&owner, availability_address.clone()).await;
    owner
        .execute(Command::ChangeMembership(MembershipChange::Remove(
            availability_id,
        )))
        .await
        .unwrap();
    next_membership(&mut availability_events).await;
    timeout(Duration::from_secs(5), async {
        loop {
            if owner
                .connection_diagnostics()
                .await
                .iter()
                .all(|peer| peer.member() != availability_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    owner
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([84; 32]), "after removal").unwrap(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        query_count(availability_dir.path(), "encrypted_messages").await,
        1
    );

    owner.shutdown().await.unwrap();
    availability.shutdown().await.unwrap();
    participant.shutdown().await.unwrap();
}

async fn next_membership(events: &mut broadcast::Receiver<Event>) -> grimoire_core::Community {
    timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                Event::MembershipChanged(community) => break community,
                Event::Fault(fault) => panic!("membership sync failed: {fault}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}

async fn connect_eventually(node: &Node, address: grimoire_core::PeerAddress) {
    timeout(Duration::from_secs(5), async {
        loop {
            if node.connect(address.clone()).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_count(dir: &Path, table: &str, expected: i64) {
    timeout(Duration::from_secs(5), async {
        loop {
            if query_count(dir, table).await == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

async fn query_count(dir: &Path, table: &str) -> i64 {
    let database = turso::Builder::new_local(&dir.join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    let sql = format!("SELECT COUNT(*) FROM {table}");
    connection
        .query(&sql, ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap()
}

async fn query_recipient_envelopes(dir: &Path, member: grimoire_core::MemberId) -> i64 {
    let database = turso::Builder::new_local(&dir.join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection
        .query(
            "SELECT COUNT(*) FROM content_key_envelopes WHERE recipient = ?1",
            [member.as_bytes().as_slice()],
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap()
}
