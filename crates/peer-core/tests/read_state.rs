use peer_core::{
    Attachment, Channel, ChannelId, ChannelKind, Command, FaultKind, MembershipChange, MessageId,
    Node, NodeConfig, TextMessage,
};

#[tokio::test]
async fn read_cursor_is_validated_monotonic_durable_and_local() {
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

    let first = MessageId::from_bytes([10; 32]);
    let second = MessageId::from_bytes([20; 32]);
    owner
        .execute(Command::PostText(TextMessage::new(first, "first").unwrap()))
        .await
        .unwrap();
    owner
        .execute(Command::ShareAttachment(
            Attachment::new(second, ChannelId::GENERAL, "second.txt", b"second".to_vec()).unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(owner.last_read(ChannelId::GENERAL).await.unwrap(), None);
    owner
        .set_last_read(ChannelId::GENERAL, second)
        .await
        .unwrap();
    owner
        .set_last_read(ChannelId::GENERAL, first)
        .await
        .unwrap();
    assert_eq!(
        owner.last_read(ChannelId::GENERAL).await.unwrap(),
        Some(second)
    );
    assert_eq!(member.last_read(ChannelId::GENERAL).await.unwrap(), None);

    let missing = owner
        .set_last_read(ChannelId::GENERAL, MessageId::from_bytes([30; 32]))
        .await
        .unwrap_err();
    assert_eq!(missing.kind(), FaultKind::Protocol);
    assert!(
        owner
            .last_read(ChannelId::from_bytes([40; 32]))
            .await
            .is_err()
    );
    assert!(owner.last_read(ChannelId::VOICE_ROOM).await.is_err());

    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    assert_eq!(
        owner.last_read(ChannelId::GENERAL).await.unwrap(),
        Some(second)
    );
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_cursor_rejects_an_item_from_another_channel() {
    let directory = tempfile::tempdir().unwrap();
    let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
    let channel = Channel::new(
        ChannelId::from_bytes([2; 32]),
        "elsewhere",
        ChannelKind::Text,
    )
    .unwrap();
    node.execute(Command::CreateChannel(channel.clone()))
        .await
        .unwrap();
    let id = MessageId::from_bytes([3; 32]);
    node.execute(Command::PostText(
        TextMessage::in_channel(id, channel.id(), "elsewhere").unwrap(),
    ))
    .await
    .unwrap();

    assert!(node.set_last_read(ChannelId::GENERAL, id).await.is_err());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_read_state_migration_marks_existing_history_read_once() {
    let directory = tempfile::tempdir().unwrap();
    let first = MessageId::from_bytes([50; 32]);
    let forgotten = MessageId::from_bytes([55; 32]);
    let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
    node.execute(Command::PostText(
        TextMessage::new(first, "existing history").unwrap(),
    ))
    .await
    .unwrap();
    node.execute(Command::ShareAttachment(
        Attachment::new(
            forgotten,
            ChannelId::GENERAL,
            "forgotten.txt",
            b"forgotten".to_vec(),
        )
        .unwrap(),
    ))
    .await
    .unwrap();
    node.execute(Command::ForgetAttachment {
        author: node.member_id(),
        id: forgotten,
    })
    .await
    .unwrap();
    node.shutdown().await.unwrap();

    let database = turso::Builder::new_local(&directory.path().join("peer.db").to_string_lossy())
        .build()
        .await
        .unwrap();
    database
        .connect()
        .unwrap()
        .execute("DROP TABLE local_read_state", ())
        .await
        .unwrap();
    drop(database);

    let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
    assert_eq!(
        node.last_read(ChannelId::GENERAL).await.unwrap(),
        Some(first)
    );
    let second = MessageId::from_bytes([60; 32]);
    node.execute(Command::PostText(
        TextMessage::new(second, "still unread").unwrap(),
    ))
    .await
    .unwrap();
    node.shutdown().await.unwrap();

    let node = Node::open(NodeConfig::new(directory.path())).await.unwrap();
    assert_eq!(
        node.last_read(ChannelId::GENERAL).await.unwrap(),
        Some(first)
    );
    node.shutdown().await.unwrap();
}
