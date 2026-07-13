use std::time::Duration;

use grimoire_core::{
    Command, Event, MembershipChange, MessageId, Node, NodeConfig, TextMessage, restore_identity,
};
use tokio::time::timeout;

#[tokio::test]
async fn encrypted_identity_backup_round_trips_without_overwrite_or_partial_restore() {
    let original_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    let backup = backup_dir.path().join("identity.pcib");
    let original = Node::open(NodeConfig::new(original_dir.path()))
        .await
        .unwrap();
    let member = original.member_id();
    let community = original.community_id();
    let peer_dir = tempfile::tempdir().unwrap();
    let peer = Node::open(
        NodeConfig::new(peer_dir.path()).community(original.community_id(), original.member_id()),
    )
    .await
    .unwrap();
    let pending_backup = backup_dir.path().join("pending.pcib");
    peer.export_identity(&pending_backup, "pending member phrase")
        .await
        .unwrap();
    let pending_restore = tempfile::tempdir().unwrap();
    assert_eq!(
        restore_identity(
            pending_restore.path(),
            &pending_backup,
            "pending member phrase"
        )
        .await
        .unwrap(),
        peer.member_id()
    );
    original.connect(peer.address()).await.unwrap();
    original
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            peer.member_id(),
        )))
        .await
        .unwrap();
    original
        .export_identity(&backup, "correct horse battery staple")
        .await
        .unwrap();
    assert!(
        original
            .export_identity(&backup, "correct horse battery staple")
            .await
            .is_err()
    );
    original.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();

    let wrong_target = tempfile::tempdir().unwrap();
    assert!(
        restore_identity(wrong_target.path(), &backup, "wrong passphrase")
            .await
            .is_err()
    );
    assert!(!wrong_target.path().join("peer.db").exists());

    let mut tampered = std::fs::read(&backup).unwrap();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    let tampered_path = backup_dir.path().join("tampered.pcib");
    std::fs::write(&tampered_path, tampered).unwrap();
    let tampered_target = tempfile::tempdir().unwrap();
    assert!(
        restore_identity(
            tampered_target.path(),
            &tampered_path,
            "correct horse battery staple"
        )
        .await
        .is_err()
    );
    assert!(!tampered_target.path().join("peer.db").exists());

    let recovered_dir = tempfile::tempdir().unwrap();
    assert_eq!(
        restore_identity(
            recovered_dir.path(),
            &backup,
            "correct horse battery staple"
        )
        .await
        .unwrap(),
        member
    );
    let recovered = Node::open(NodeConfig::new(recovered_dir.path()))
        .await
        .unwrap();
    assert_eq!(recovered.member_id(), member);
    assert_eq!(recovered.community_id(), community);
    let peer = Node::open(NodeConfig::new(peer_dir.path())).await.unwrap();
    assert!(recovered.community_invite().await.is_err());
    assert!(
        recovered
            .execute(Command::ChangeMembership(MembershipChange::Admit(
                peer.member_id(),
            )))
            .await
            .is_err()
    );
    let mut recovered_events = recovered.subscribe();
    let mut peer_events = peer.subscribe();
    recovered.connect(peer.address()).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                recovered_events.recv().await.unwrap(),
                Event::MembershipChanged(_)
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    recovered.community_invite().await.unwrap();
    recovered
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([61; 32]), "recovered owner").unwrap(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(peer_events.recv().await.unwrap(), Event::TextStored(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    recovered.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();

    let initialized_dir = tempfile::tempdir().unwrap();
    Node::open(NodeConfig::new(initialized_dir.path()))
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    assert!(
        restore_identity(
            initialized_dir.path(),
            &backup,
            "correct horse battery staple"
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn recovered_owner_restores_pre_backup_history_from_a_peer() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    let backup = backup_dir.path().join("identity.pcib");
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let owner_id = owner.member_id();
    let community_id = owner.community_id();
    let member = Node::open(NodeConfig::new(member_dir.path()).community(community_id, owner_id))
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

    let channel = grimoire_core::Channel::new(
        grimoire_core::ChannelId::from_bytes([62; 32]),
        "recovery",
        grimoire_core::ChannelKind::Text,
    )
    .unwrap();
    let message = TextMessage::in_channel(
        MessageId::from_bytes([63; 32]),
        channel.id(),
        "before backup",
    )
    .unwrap();
    owner
        .execute(Command::CreateChannel(channel.clone()))
        .await
        .unwrap();
    owner
        .execute(Command::PostText(message.clone()))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(member_events.recv().await.unwrap(), Event::TextStored(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    owner
        .export_identity(&backup, "history recovery phrase")
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    member.shutdown().await.unwrap();

    let recovered_dir = tempfile::tempdir().unwrap();
    restore_identity(recovered_dir.path(), &backup, "history recovery phrase")
        .await
        .unwrap();
    let recovered = Node::open(NodeConfig::new(recovered_dir.path()))
        .await
        .unwrap();
    let member = Node::open(NodeConfig::new(member_dir.path()))
        .await
        .unwrap();
    let mut recovered_events = recovered.subscribe();
    recovered.connect(member.address()).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(recovered_events.recv().await.unwrap(), Event::TextStored(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();

    let snapshot = recovered.snapshot().await.unwrap();
    assert!(snapshot.channels().contains(&channel));
    assert!(
        snapshot
            .messages()
            .iter()
            .any(|authored| { authored.author() == owner_id && authored.message() == &message })
    );
    recovered.shutdown().await.unwrap();
    member.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovered_member_catches_up_after_an_offline_key_rotation() {
    let owner_dir = tempfile::tempdir().unwrap();
    let member_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    let backup = backup_dir.path().join("member.pcib");
    let owner = Node::open(NodeConfig::new(owner_dir.path())).await.unwrap();
    let member = Node::open(
        NodeConfig::new(member_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    let member_id = member.member_id();
    let mut member_events = member.subscribe();
    owner.connect(member.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            member_id,
        )))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                member_events.recv().await.unwrap(),
                Event::MembershipChanged(_)
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    member
        .export_identity(&backup, "member recovery phrase")
        .await
        .unwrap();
    member.shutdown().await.unwrap();

    let newcomer_dir = tempfile::tempdir().unwrap();
    let newcomer = Node::open(
        NodeConfig::new(newcomer_dir.path()).community(owner.community_id(), owner.member_id()),
    )
    .await
    .unwrap();
    owner.connect(newcomer.address()).await.unwrap();
    owner
        .execute(Command::ChangeMembership(MembershipChange::Admit(
            newcomer.member_id(),
        )))
        .await
        .unwrap();
    let missed = TextMessage::new(MessageId::from_bytes([64; 32]), "after rotation").unwrap();
    owner
        .execute(Command::PostText(missed.clone()))
        .await
        .unwrap();

    let recovered_dir = tempfile::tempdir().unwrap();
    restore_identity(recovered_dir.path(), &backup, "member recovery phrase")
        .await
        .unwrap();
    let recovered = Node::open(NodeConfig::new(recovered_dir.path()))
        .await
        .unwrap();
    let mut recovered_events = recovered.subscribe();
    recovered.connect(owner.address()).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(recovered_events.recv().await.unwrap(), Event::TextStored(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        recovered
            .snapshot()
            .await
            .unwrap()
            .messages()
            .iter()
            .any(|authored| authored.message() == &missed)
    );

    owner.shutdown().await.unwrap();
    newcomer.shutdown().await.unwrap();
    recovered.shutdown().await.unwrap();
}
