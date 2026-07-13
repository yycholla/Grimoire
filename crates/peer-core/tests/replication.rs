use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, endpoint::presets};
use peer_core::{
    Attachment, Channel, ChannelId, ChannelKind, Command, ConnectionPathKind, DisplayName, Event,
    FaultKind, MemberId, MembershipChange, MessageId, Node, NodeConfig, TextMessage, VoiceFrame,
    VoicePresence, VoiceStreamId,
};

#[tokio::test]
async fn encrypted_attachment_is_replicated_and_persisted() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    let mut events = member.subscribe();
    let attachment = Attachment::new(
        MessageId::from_bytes([70; 32]),
        ChannelId::GENERAL,
        "notes.txt",
        b"private attachment bytes".to_vec(),
    )
    .unwrap();
    owner
        .execute(Command::ShareAttachment(attachment.clone()))
        .await
        .unwrap();
    let received = timeout(Duration::from_secs(5), async {
        loop {
            if let Event::AttachmentStored(received) = events.recv().await.unwrap() {
                break received;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(received.author(), owner.member_id());
    assert_eq!(received.attachment(), &attachment);
    assert_eq!(
        member.snapshot().await.unwrap().attachments()[0].attachment(),
        &attachment
    );
    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&member_dir.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let mut rows = database
        .connect()
        .unwrap()
        .query(
            "SELECT ciphertext FROM encrypted_attachments WHERE id = ?1",
            [attachment.id().as_bytes().as_slice()],
        )
        .await
        .unwrap();
    let ciphertext: Vec<u8> = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert!(
        !ciphertext
            .windows(attachment.bytes().len())
            .any(|window| window == attachment.bytes())
    );
    drop(rows);
    drop(database);

    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    member
        .execute(Command::ForgetAttachment {
            author: owner.member_id(),
            id: attachment.id(),
        })
        .await
        .unwrap();
    assert!(member.snapshot().await.unwrap().attachments().is_empty());
    member.shutdown().await.unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    member.connect(owner.address()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(member.snapshot().await.unwrap().attachments().is_empty());
    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_member_catches_up_attachment_from_removed_author() {
    let owner_dir = tempfile::tempdir().unwrap();
    let author_dir = tempfile::tempdir().unwrap();
    let reader_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let author = Node::open(
        NodeConfig::new(author_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let reader = Node::open(
        NodeConfig::new(reader_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(author.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            author.member_id(),
        )))
        .await
        .unwrap();
    owner.connect(reader.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            reader.member_id(),
        )))
        .await
        .unwrap();
    reader.shutdown().await.unwrap();

    let attachment = Attachment::new(
        MessageId::from_bytes([71; 32]),
        ChannelId::GENERAL,
        "before-removal.txt",
        b"history".to_vec(),
    )
    .unwrap();
    author
        .execute(Command::ShareAttachment(attachment.clone()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    owner
        .execute(Command::ChangeMembership(MembershipChange::Remove(
            author.member_id(),
        )))
        .await
        .unwrap();

    let reader = Node::open(NodeConfig::new(reader_dir.path()))
        .await
        .unwrap();
    let mut events = reader.subscribe();
    reader.connect(owner.address()).await.unwrap();
    let received = timeout(Duration::from_secs(5), async {
        loop {
            if let Event::AttachmentStored(received) = events.recv().await.unwrap() {
                break received;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(received.author(), author.member_id());
    assert_eq!(received.attachment(), &attachment);
    owner.shutdown().await.unwrap();
    author.shutdown().await.unwrap();
    reader.shutdown().await.unwrap();
}
use prost::Message;
use tokio::sync::broadcast;
use tokio::time::timeout;

#[tokio::test]
async fn display_name_is_private_persistent_and_offline_synced() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let member_id = member.member_id();
    owner
        .execute(Command::SetDisplayName(
            DisplayName::new("Owner Alice").unwrap(),
        ))
        .await
        .unwrap();
    assert!(
        member
            .execute(Command::SetDisplayName(DisplayName::new("Bob").unwrap()))
            .await
            .is_err()
    );
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member_id,
        )))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if member
                .snapshot()
                .await
                .unwrap()
                .display_names()
                .get(&owner.member_id())
                .is_some_and(|name| name.as_str() == "Owner Alice")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut owner_events = owner.subscribe();
    member
        .execute(Command::SetDisplayName(DisplayName::new("Bob").unwrap()))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                owner_events.recv().await.unwrap(),
                Event::DisplayNameChanged { member, ref name }
                    if member == member_id && name.as_str() == "Bob"
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();

    owner.shutdown().await.unwrap();
    member
        .execute(Command::SetDisplayName(DisplayName::new("Robert").unwrap()))
        .await
        .unwrap();
    member.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&member_dir.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let mut rows = database
        .connect()
        .unwrap()
        .query(
            "SELECT ciphertext FROM member_profiles WHERE member = ?1",
            [member_id.as_bytes().as_slice()],
        )
        .await
        .unwrap();
    let ciphertext: Vec<u8> = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert!(!ciphertext.windows(6).any(|window| window == b"Robert"));
    drop(rows);
    drop(database);

    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut owner_events = owner.subscribe();
    owner.connect(member.address()).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                owner_events.recv().await.unwrap(),
                Event::DisplayNameChanged { member, ref name }
                    if member == member_id && name.as_str() == "Robert"
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        owner.snapshot().await.unwrap().display_names()[&member_id].as_str(),
        "Robert"
    );
    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn message_is_persisted_and_replicated_between_two_nodes() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path())).await.unwrap();
    let first_member = first.member_id();
    let second = Node::open(
        NodeConfig::new(second_dir.path()).community(first.community_id(), first_member),
    )
    .await
    .unwrap();
    let second_member = second.member_id();
    first.connect(second.address()).await.unwrap();
    first
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            second_member,
        )))
        .await
        .unwrap();
    let mut second_events = second.subscribe();

    let message = TextMessage::new(MessageId::from_bytes([7; 32]), "hello peer").unwrap();
    first
        .execute(Command::PostText(message.clone()))
        .await
        .unwrap();

    let Event::TextStored(authored) = timeout(Duration::from_secs(5), second_events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected authenticated text event");
    };
    assert_eq!(authored.author(), first_member);
    assert_eq!(authored.message(), &message);
    assert_eq!(
        second.snapshot().await.unwrap().messages()[0].author(),
        first_member
    );
    assert_eq!(
        second.snapshot().await.unwrap().messages()[0].message(),
        &message
    );

    second.shutdown().await.unwrap();
    let reopened = Node::open(NodeConfig::new(second_dir.path()))
        .await
        .unwrap();
    assert_eq!(reopened.member_id(), second_member);
    assert_eq!(
        reopened.snapshot().await.unwrap().messages()[0].author(),
        first_member
    );
    assert_eq!(
        reopened.snapshot().await.unwrap().messages()[0].message(),
        &message
    );

    first.shutdown().await.unwrap();
    reopened.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&second_dir.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    let mut rows = connection
        .query(
            "SELECT ciphertext FROM encrypted_messages WHERE id = ?1",
            [message.id().as_bytes().as_slice()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let ciphertext: Vec<u8> = row.get(0).unwrap();
    assert!(!ciphertext.is_empty());
    assert!(
        !ciphertext
            .windows(message.body().len())
            .any(|window| window == message.body().as_bytes())
    );
}

#[tokio::test]
async fn offline_message_catches_up_after_reconnect() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    member.shutdown().await.unwrap();

    let missed = TextMessage::new(MessageId::from_bytes([21; 32]), "while offline").unwrap();
    owner
        .execute(Command::PostText(missed.clone()))
        .await
        .unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut events = member.subscribe();
    member.connect(owner.address()).await.unwrap();

    assert_eq!(next_text(&mut events).await.message(), &missed);
    assert_eq!(member.snapshot().await.unwrap().messages().len(), 1);

    member.shutdown().await.unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    assert_eq!(member.snapshot().await.unwrap().messages().len(), 1);
    assert!(
        timeout(Duration::from_millis(250), next_text(&mut events))
            .await
            .is_err()
    );

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn owner_channels_are_persisted_and_synced_before_their_messages() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let custom_id = ChannelId::from_bytes([8; 32]);
    let custom = Channel::new(custom_id, "projects", ChannelKind::Text).unwrap();
    owner
        .execute(Command::CreateChannel(custom.clone()))
        .await
        .unwrap();
    assert_eq!(owner.snapshot().await.unwrap().channels().len(), 3);

    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();

    let message =
        TextMessage::in_channel(MessageId::from_bytes([31; 32]), custom_id, "on topic").unwrap();
    owner
        .execute(Command::PostText(message.clone()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut events).await.message(), &message);
    let snapshot = member.snapshot().await.unwrap();
    assert_eq!(snapshot.channels().len(), 3);
    assert!(snapshot.channels().contains(&custom));

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_owner_can_create_channels() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let result = member
        .execute(Command::CreateChannel(
            Channel::new(ChannelId::from_bytes([9; 32]), "forged", ChannelKind::Text).unwrap(),
        ))
        .await;
    assert_eq!(result.unwrap_err().kind(), FaultKind::Authorization);
    assert!(member.snapshot().await.unwrap().channels().is_empty());

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn operations_require_a_channel_of_the_right_kind() {
    let data_dir = tempfile::tempdir().unwrap();
    let node = Node::open(NodeConfig::new(data_dir.path())).await.unwrap();
    let text_in_voice = TextMessage::in_channel(
        MessageId::from_bytes([32; 32]),
        ChannelId::VOICE_ROOM,
        "wrong room",
    )
    .unwrap();
    assert_eq!(
        node.execute(Command::PostText(text_in_voice))
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    let voice_in_text = VoiceFrame::in_channel(
        VoiceStreamId::from_bytes([5; 16]),
        ChannelId::GENERAL,
        0,
        vec![1],
    )
    .unwrap();
    assert_eq!(
        node.execute(Command::SendVoice(voice_in_text))
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn independently_written_histories_converge_in_message_id_order() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let owner_id = owner.member_id();
    let member =
        Node::open(NodeConfig::new(member_dir.path()).community(owner.community_id(), owner_id))
            .await
            .unwrap();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    owner.shutdown().await.unwrap();

    member
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([1; 32]), "from member").unwrap(),
        ))
        .await
        .unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    owner
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([9; 32]), "from owner").unwrap(),
        ))
        .await
        .unwrap();
    let mut member_events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    next_text(&mut member_events).await;

    let owner_messages = owner.snapshot().await.unwrap();
    let member_messages = member.snapshot().await.unwrap();
    let owner_ids = owner_messages
        .messages()
        .iter()
        .map(|authored| authored.message().id())
        .collect::<Vec<_>>();
    let member_ids = member_messages
        .messages()
        .iter()
        .map(|authored| authored.message().id())
        .collect::<Vec<_>>();
    assert_eq!(owner_ids, member_ids);
    assert_eq!(
        owner_ids,
        vec![
            MessageId::from_bytes([1; 32]),
            MessageId::from_bytes([9; 32])
        ]
    );

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn message_ids_cannot_replace_existing_operations() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut member_events = member.subscribe();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    let id = MessageId::from_bytes([42; 32]);
    owner
        .execute(Command::PostText(TextMessage::new(id, "original").unwrap()))
        .await
        .unwrap();
    next_text(&mut member_events).await;

    let error = owner
        .execute(Command::PostText(
            TextMessage::new(id, "replacement").unwrap(),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), FaultKind::Protocol);
    let mut owner_events = owner.subscribe();
    member
        .execute(Command::PostText(
            TextMessage::new(id, "independent author").unwrap(),
        ))
        .await
        .unwrap();
    next_text(&mut owner_events).await;
    assert_eq!(owner.snapshot().await.unwrap().messages().len(), 2);
    let snapshot = member.snapshot().await.unwrap();
    assert_eq!(snapshot.messages().len(), 2);
    assert!(
        snapshot
            .messages()
            .iter()
            .any(|message| message.message().body() == "original")
    );
    assert!(
        snapshot
            .messages()
            .iter()
            .all(|message| message.message().body() != "replacement")
    );

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn incoming_peer_identity_can_be_used_for_admission() {
    let owner_dir = tempfile::tempdir().unwrap();
    let joining_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let joining = Node::open(
        NodeConfig::new(joining_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut owner_events = owner.subscribe();

    joining.connect(owner.address()).await.unwrap();
    let Event::PeerConnected(discovered_member) =
        timeout(Duration::from_secs(5), owner_events.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("expected connected peer identity");
    };
    assert_eq!(discovered_member, joining.member_id());

    let mut joining_events = joining.subscribe();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            discovered_member,
        )))
        .await
        .unwrap();
    let community = next_membership(&mut joining_events).await;
    assert!(community.contains(discovered_member));

    owner.shutdown().await.unwrap();
    joining.shutdown().await.unwrap();
}

#[tokio::test]
async fn community_membership_controls_text_replication() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let outsider_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let outsider = Node::open(
        NodeConfig::new(outsider_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut member_events = member.subscribe();
    owner.connect(member.address()).await.unwrap();
    assert_eq!(
        connected_member(&mut member_events).await,
        owner.member_id()
    );

    owner
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([6; 32]), "not yet").unwrap(),
        ))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(200), member_events.recv())
            .await
            .is_err()
    );
    assert!(member.snapshot().await.unwrap().messages().is_empty());

    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    let community = next_membership(&mut member_events).await;
    assert!(community.contains(member.member_id()));
    assert!(member.snapshot().await.unwrap().messages().is_empty());

    let mut owner_events = owner.subscribe();
    member
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([8; 32]), "welcome").unwrap(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(5), owner_events.recv())
            .await
            .unwrap()
            .unwrap(),
        Event::TextStored(_)
    ));

    let mut owner_events = owner.subscribe();
    outsider.connect(owner.address()).await.unwrap();
    assert_eq!(
        connected_member(&mut owner_events).await,
        outsider.member_id()
    );
    let fault = outsider
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([9; 32]), "intrusion").unwrap(),
        ))
        .await
        .unwrap_err();
    assert_eq!(fault.kind(), FaultKind::Authorization);
    assert_eq!(owner.snapshot().await.unwrap().messages().len(), 2);

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
    outsider.shutdown().await.unwrap();
}

#[tokio::test]
async fn removal_stops_future_content_without_erasing_history() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut member_events = member.subscribe();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    while !matches!(
        member_events.recv().await.unwrap(),
        Event::MembershipChanged(_)
    ) {}

    owner
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([51; 32]), "before removal").unwrap(),
        ))
        .await
        .unwrap();
    next_text(&mut member_events).await;
    owner
        .execute(Command::ChangeMembership(MembershipChange::Remove(
            member.member_id(),
        )))
        .await
        .unwrap();
    while !matches!(
        member_events.recv().await.unwrap(),
        Event::MembershipChanged(_)
    ) {}

    member.shutdown().await.unwrap();

    let offline_id = MessageId::from_bytes([52; 32]);
    owner
        .execute(Command::PostText(
            TextMessage::new(offline_id, "while removed offline").unwrap(),
        ))
        .await
        .unwrap();

    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut member_events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    let stale_voice =
        VoiceFrame::new(VoiceStreamId::from_bytes([6; 16]), 0, vec![1, 2, 3]).unwrap();
    assert_eq!(
        member
            .execute(Command::SendVoice(stale_voice))
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    let live_id = MessageId::from_bytes([53; 32]);
    owner
        .execute(Command::PostText(
            TextMessage::new(live_id, "after removed reconnect").unwrap(),
        ))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(500), async {
            loop {
                if matches!(member_events.recv().await.unwrap(), Event::TextStored(_)) {
                    break;
                }
            }
        })
        .await
        .is_err()
    );
    let messages = member.snapshot().await.unwrap();
    assert_eq!(messages.messages().len(), 1);
    assert_eq!(messages.messages()[0].message().body(), "before removal");

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&member_dir.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    let mut rows = connection
        .query(
            "SELECT COUNT(*) FROM encrypted_messages WHERE id IN (?1, ?2)",
            [
                offline_id.as_bytes().as_slice(),
                live_id.as_bytes().as_slice(),
            ],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 0);
}

#[tokio::test]
async fn voice_frame_is_delivered_between_two_nodes() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path())).await.unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path()).community(first.community_id(), first.member_id()),
    )
    .await
    .unwrap();
    first.connect(second.address()).await.unwrap();
    first
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            second.member_id(),
        )))
        .await
        .unwrap();
    first
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    second
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    let mut first_events = first.subscribe();
    let mut second_events = second.subscribe();

    let frame = VoiceFrame::new(VoiceStreamId::from_bytes([3; 16]), 7, vec![1, 2, 3]).unwrap();
    first
        .execute(Command::SendVoice(frame.clone()))
        .await
        .unwrap();

    let Event::VoiceReceived(authored) = timeout(Duration::from_secs(5), second_events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected authenticated voice");
    };
    assert_eq!(authored.author(), first.member_id());
    assert_eq!(authored.frame(), &frame);

    let reply = VoiceFrame::new(VoiceStreamId::from_bytes([3; 16]), 8, vec![4, 5, 6]).unwrap();
    second
        .execute(Command::SendVoice(reply.clone()))
        .await
        .unwrap();
    let Event::VoiceReceived(authored) = timeout(Duration::from_secs(5), first_events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected authenticated voice reply");
    };
    assert_eq!(authored.author(), second.member_id());
    assert_eq!(authored.frame(), &reply);

    let sender = Endpoint::bind(presets::Minimal).await.unwrap();
    let address: EndpointAddr = serde_json::from_str(&first.address().to_string()).unwrap();
    let connection = sender
        .connect(address, b"peer-community/operations/2")
        .await
        .unwrap();
    connected_member(&mut first_events).await;
    connection
        .send_datagram(
            RawVoiceFrame {
                stream_id: vec![9; 16],
                sequence: 0,
                payload: vec![9],
            }
            .encode_to_vec()
            .into(),
        )
        .unwrap();
    let Event::Fault(fault) = timeout(Duration::from_secs(5), first_events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected outsider voice rejection");
    };
    assert_eq!(fault.kind(), FaultKind::Authorization);

    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn voice_presence_replicates_live_transitions_and_late_peer_state() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let late_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    let mut owner_events = owner.subscribe();
    let mut member_events = member.subscribe();

    member
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut member_events).await,
        (
            ChannelId::VOICE_ROOM,
            member.member_id(),
            VoicePresence::Joined
        )
    );
    assert_eq!(
        next_voice_presence(&mut owner_events).await,
        (
            ChannelId::VOICE_ROOM,
            member.member_id(),
            VoicePresence::Joined
        )
    );
    member
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Muted(true),
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut owner_events).await.2,
        VoicePresence::Muted(true)
    );
    assert_eq!(
        next_voice_presence(&mut member_events).await.2,
        VoicePresence::Muted(true)
    );
    let late = Node::open(
        NodeConfig::new(late_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut late_events = late.subscribe();
    late.connect(owner.address()).await.unwrap();
    late.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            late.member_id(),
        )))
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut late_events).await,
        (
            ChannelId::VOICE_ROOM,
            member.member_id(),
            VoicePresence::Joined
        )
    );
    assert_eq!(
        next_voice_presence(&mut late_events).await.2,
        VoicePresence::Muted(true)
    );
    late.shutdown().await.unwrap();
    assert_eq!(
        member
            .execute(Command::SetVoicePresence {
                channel: ChannelId::GENERAL,
                state: VoicePresence::Joined,
            })
            .await
            .unwrap_err()
            .kind(),
        FaultKind::Authorization
    );
    member
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Left,
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut owner_events).await.2,
        VoicePresence::Left
    );
    assert_eq!(
        next_voice_presence(&mut member_events).await,
        (
            ChannelId::VOICE_ROOM,
            member.member_id(),
            VoicePresence::Left
        )
    );

    owner
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut member_events).await.1,
        owner.member_id()
    );
    assert_eq!(
        next_voice_presence(&mut owner_events).await,
        (
            ChannelId::VOICE_ROOM,
            owner.member_id(),
            VoicePresence::Joined
        )
    );
    member.shutdown().await.unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut member_events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    assert_eq!(
        next_voice_presence(&mut member_events).await,
        (
            ChannelId::VOICE_ROOM,
            owner.member_id(),
            VoicePresence::Joined
        )
    );

    member
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut owner_events).await.1,
        member.member_id()
    );
    let member_id = member.member_id();
    member.shutdown().await.unwrap();
    assert_eq!(
        next_voice_presence(&mut owner_events).await,
        (ChannelId::VOICE_ROOM, member_id, VoicePresence::Left)
    );

    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn voice_presence_stops_at_membership_and_disconnect_boundaries() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let outsider_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let outsider = Node::open(
        NodeConfig::new(outsider_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    outsider.connect(owner.address()).await.unwrap();
    let mut member_events = member.subscribe();
    let mut outsider_events = outsider.subscribe();

    owner
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    assert_eq!(
        next_voice_presence(&mut member_events).await.1,
        owner.member_id()
    );
    assert!(
        timeout(Duration::from_millis(200), async {
            loop {
                if matches!(
                    outsider_events.recv().await.unwrap(),
                    Event::VoicePresence { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .is_err()
    );

    owner
        .execute(Command::ChangeMembership(MembershipChange::Remove(
            member.member_id(),
        )))
        .await
        .unwrap();
    next_membership(&mut member_events).await;
    owner
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Muted(true),
        })
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(200), async {
            loop {
                if matches!(
                    member_events.recv().await.unwrap(),
                    Event::VoicePresence { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .is_err()
    );

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
    outsider.shutdown().await.unwrap();
}

#[tokio::test]
async fn transport_identity_matches_durable_member_identity() {
    let data_dir = tempfile::tempdir().unwrap();
    let node = Node::open(NodeConfig::new(data_dir.path())).await.unwrap();
    let member = node.member_id();
    assert_eq!(node.address().member_id(), member);
    let invite = node
        .community_invite()
        .await
        .unwrap()
        .to_string()
        .parse::<peer_core::CommunityInvite>()
        .unwrap();
    assert_eq!(invite.community_id(), node.community_id());
    assert_eq!(invite.owner_address().member_id(), member);
    node.shutdown().await.unwrap();

    let reopened = Node::open(NodeConfig::new(data_dir.path())).await.unwrap();
    assert_eq!(reopened.member_id(), member);
    assert_eq!(reopened.address().member_id(), member);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_connections_report_direct_path_diagnostics() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path())).await.unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path()).community(first.community_id(), first.member_id()),
    )
    .await
    .unwrap();
    first.connect(second.address()).await.unwrap();
    let diagnostics = first.connection_diagnostics().await;
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].member(), second.member_id());
    assert!(
        diagnostics[0]
            .paths()
            .iter()
            .any(|path| path.kind() == ConnectionPathKind::Direct && path.is_selected())
    );
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnect_replaces_stale_connection_diagnostics() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path())).await.unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path()).community(first.community_id(), first.member_id()),
    )
    .await
    .unwrap();
    first.connect(second.address()).await.unwrap();
    second.shutdown().await.unwrap();
    let second = Node::open(NodeConfig::new(second_dir.path()))
        .await
        .unwrap();
    first.connect(second.address()).await.unwrap();
    let diagnostics = first.connection_diagnostics().await;
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].member(), second.member_id());
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn simultaneous_connections_converge_to_one_per_peer() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path())).await.unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path()).community(first.community_id(), first.member_id()),
    )
    .await
    .unwrap();

    let (first_result, second_result) = tokio::join!(
        first.connect(second.address()),
        second.connect(first.address())
    );
    first_result.unwrap();
    second_result.unwrap();

    assert_eq!(first.connection_diagnostics().await.len(), 1);
    assert_eq!(second.connection_diagnostics().await.len(), 1);
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_registration_stays_ephemeral_until_admission() {
    let owner_dir = tempfile::tempdir().unwrap();
    let joining_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let joining = Node::open(
        NodeConfig::new(joining_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    joining.connect(owner.address()).await.unwrap();
    owner.shutdown().await.unwrap();
    joining.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&owner_dir.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    let mut rows = database
        .connect()
        .unwrap()
        .query("SELECT COUNT(*) FROM key_registrations", ())
        .await
        .unwrap();
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(count, 1, "only the owner's registration is durable");
}

#[tokio::test]
async fn unadmitted_peer_does_not_receive_community_history() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let outsider_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    member.connect(owner.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    owner
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([73; 32]), "private history").unwrap(),
        ))
        .await
        .unwrap();

    let outsider = Node::open(
        NodeConfig::new(outsider_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    outsider.connect(owner.address()).await.unwrap();
    let snapshot = outsider.snapshot().await.unwrap();
    assert!(!snapshot.members().contains(&member.member_id()));
    assert!(snapshot.channels().is_empty());
    assert!(snapshot.messages().is_empty());
    assert!(snapshot.attachments().is_empty());
    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
    outsider.shutdown().await.unwrap();
}

#[tokio::test]
async fn control_metadata_only_reaches_current_members_and_the_transition_subject() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let outsider_dir = tempfile::tempdir().unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let outsider = Node::open(
        NodeConfig::new(outsider_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let mut member_events = member.subscribe();
    member.connect(owner.address()).await.unwrap();
    outsider.connect(owner.address()).await.unwrap();

    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member.member_id(),
        )))
        .await
        .unwrap();
    assert!(
        next_membership(&mut member_events)
            .await
            .contains(member.member_id())
    );

    let admitted_channel = Channel::new(
        ChannelId::from_bytes([74; 32]),
        "members-only",
        ChannelKind::Text,
    )
    .unwrap();
    owner
        .execute(Command::CreateChannel(admitted_channel.clone()))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                member_events.recv().await.unwrap(),
                Event::ChannelCreated(ref channel) if channel == &admitted_channel
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();

    let outsider_snapshot = outsider.snapshot().await.unwrap();
    assert!(!outsider_snapshot.members().contains(&member.member_id()));
    assert!(outsider_snapshot.channels().is_empty());

    owner
        .execute(Command::ChangeMembership(MembershipChange::Remove(
            member.member_id(),
        )))
        .await
        .unwrap();
    assert!(
        !next_membership(&mut member_events)
            .await
            .contains(member.member_id())
    );

    let post_removal_channel = Channel::new(
        ChannelId::from_bytes([75; 32]),
        "after-removal",
        ChannelKind::Text,
    )
    .unwrap();
    owner
        .execute(Command::CreateChannel(post_removal_channel.clone()))
        .await
        .unwrap();
    assert!(
        !member
            .snapshot()
            .await
            .unwrap()
            .channels()
            .contains(&post_removal_channel)
    );
    let outsider_snapshot = outsider.snapshot().await.unwrap();
    assert!(!outsider_snapshot.members().contains(&member.member_id()));
    assert!(outsider_snapshot.channels().is_empty());

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
    outsider.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_message_signature_is_rejected() {
    let receiver_dir = tempfile::tempdir().unwrap();
    let receiver = Node::open(NodeConfig::new(receiver_dir.path()))
        .await
        .unwrap();
    let mut events = receiver.subscribe();
    let address: EndpointAddr = serde_json::from_str(&receiver.address().to_string()).unwrap();
    let sender = Endpoint::bind(presets::Minimal).await.unwrap();
    let connection = sender
        .connect(address, b"peer-community/operations/2")
        .await
        .unwrap();
    connected_member(&mut events).await;
    let (mut send, _) = connection.open_bi().await.unwrap();
    let operation = RawOperation {
        text: Some(ForgedTextMessage {
            id: vec![1; 32],
            body: "forged".to_owned(),
            author: vec![2; 32],
            signature: vec![0; 64],
        }),
        membership: None,
    };
    send.write_all(&operation.encode_to_vec()).await.unwrap();
    send.finish().unwrap();

    let Event::Fault(fault) = timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected invalid signature fault");
    };
    assert_eq!(fault.kind(), FaultKind::Protocol);
    assert!(receiver.snapshot().await.unwrap().messages().is_empty());

    receiver.shutdown().await.unwrap();
}

#[tokio::test]
async fn voice_frame_reaches_a_four_node_mesh() {
    let dirs = (0..4)
        .map(|_| tempfile::tempdir().unwrap())
        .collect::<Vec<_>>();
    let mut nodes = vec![Node::open(NodeConfig::new(dirs[0].path())).await.unwrap()];
    let owner = nodes[0].member_id();
    let community_id = nodes[0].community_id();
    for dir in dirs.iter().skip(1) {
        nodes.push(
            Node::open(NodeConfig::new(dir.path()).community(community_id, owner))
                .await
                .unwrap(),
        );
    }
    for dialer in 1..nodes.len() {
        for receiver in 0..dialer {
            nodes[dialer]
                .connect(nodes[receiver].address())
                .await
                .unwrap();
        }
    }
    for member in nodes.iter().skip(1).map(Node::member_id) {
        nodes[0]
            .execute(Command::ChangeMembership(MembershipChange::Admit(member)))
            .await
            .unwrap();
    }
    for node in &nodes {
        node.execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    }
    let mut events = nodes.iter().map(Node::subscribe).collect::<Vec<_>>();

    let first = VoiceFrame::new(VoiceStreamId::from_bytes([4; 16]), 0, vec![1]).unwrap();
    nodes[3]
        .execute(Command::SendVoice(first.clone()))
        .await
        .unwrap();
    for receiver in events.iter_mut().take(3) {
        let Event::VoiceReceived(authored) = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected authenticated mesh voice");
        };
        assert_eq!(authored.author(), nodes[3].member_id());
        assert_eq!(authored.frame(), &first);
    }

    let reply = VoiceFrame::new(VoiceStreamId::from_bytes([1; 16]), 1, vec![2]).unwrap();
    nodes[0]
        .execute(Command::SendVoice(reply.clone()))
        .await
        .unwrap();
    for receiver in events.iter_mut().skip(1) {
        let Event::VoiceReceived(authored) = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected authenticated mesh reply");
        };
        assert_eq!(authored.author(), nodes[0].member_id());
        assert_eq!(authored.frame(), &reply);
    }

    for node in nodes {
        node.shutdown().await.unwrap();
    }
}

#[derive(Clone, PartialEq, Message)]
struct ForgedTextMessage {
    #[prost(bytes = "vec", tag = "1")]
    id: Vec<u8>,
    #[prost(string, tag = "2")]
    body: String,
    #[prost(bytes = "vec", tag = "3")]
    author: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    signature: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct RawOperation {
    #[prost(message, optional, tag = "1")]
    text: Option<ForgedTextMessage>,
    #[prost(message, optional, tag = "2")]
    membership: Option<RawMembershipChange>,
}

#[derive(Clone, PartialEq, Message)]
struct RawMembershipChange {
    #[prost(uint64, tag = "1")]
    revision: u64,
    #[prost(bytes = "vec", tag = "2")]
    member: Vec<u8>,
    #[prost(bool, tag = "3")]
    admitted: bool,
    #[prost(bytes = "vec", tag = "4")]
    signature: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct RawVoiceFrame {
    #[prost(bytes = "vec", tag = "1")]
    stream_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    sequence: u64,
    #[prost(bytes = "vec", tag = "3")]
    payload: Vec<u8>,
}

async fn connected_member(events: &mut broadcast::Receiver<Event>) -> MemberId {
    let Event::PeerConnected(member) = timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected connected peer identity");
    };
    member
}

async fn next_text(events: &mut broadcast::Receiver<Event>) -> peer_core::AuthoredText {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Event::TextStored(message) = events.recv().await.unwrap() {
                break message;
            }
        }
    })
    .await
    .unwrap()
}

async fn next_membership(events: &mut broadcast::Receiver<Event>) -> peer_core::Community {
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

async fn next_voice_presence(
    events: &mut broadcast::Receiver<Event>,
) -> (ChannelId, MemberId, VoicePresence) {
    timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                Event::VoicePresence {
                    channel,
                    member,
                    state,
                } => break (channel, member, state),
                Event::Fault(fault) => panic!("voice presence failed: {fault}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}
