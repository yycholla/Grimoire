use std::time::Duration;

use peer_core::{
    Attachment, ChannelId, Command, ConnectionPathKind, Event, MembershipChange, MessageId, Node,
    NodeConfig, TextMessage, VoiceFrame, VoicePresence, VoiceStreamId,
};
use tokio::time::timeout;

#[tokio::test]
#[ignore = "requires access to the public N0 relay network"]
async fn two_nodes_connect_over_a_forced_relay_path() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Node::open(NodeConfig::new(first_dir.path()).relay_only())
        .await
        .unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path())
            .community(first.community_id(), first.member_id())
            .relay_only(),
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
    tokio::time::sleep(Duration::from_secs(1)).await;
    let diagnostics = first.connection_diagnostics().await;
    assert!(diagnostics[0].paths().iter().any(|path| {
        path.kind() == ConnectionPathKind::Relay && path.is_selected() && !path.rtt().is_zero()
    }));

    let mut events = second.subscribe();
    first
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([80; 32]), "relay text").unwrap(),
        ))
        .await
        .unwrap();
    first
        .execute(Command::ShareAttachment(
            Attachment::new(
                MessageId::from_bytes([81; 32]),
                ChannelId::GENERAL,
                "relay.txt",
                b"relay attachment".to_vec(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    first
        .execute(Command::SetVoicePresence {
            channel: ChannelId::VOICE_ROOM,
            state: VoicePresence::Joined,
        })
        .await
        .unwrap();
    first
        .execute(Command::SendVoice(
            VoiceFrame::new(VoiceStreamId::from_bytes([8; 16]), 0, vec![1, 2, 3]).unwrap(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(10), async {
        let mut text = false;
        let mut attachment = false;
        let mut voice = false;
        while !(text && attachment && voice) {
            match events.recv().await.unwrap() {
                Event::TextStored(_) => text = true,
                Event::AttachmentStored(_) => attachment = true,
                Event::VoiceReceived(_) => voice = true,
                Event::Fault(error) => panic!("relay operation failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    second.shutdown().await.unwrap();
    first
        .execute(Command::PostText(
            TextMessage::new(MessageId::from_bytes([82; 32]), "relay offline").unwrap(),
        ))
        .await
        .unwrap();
    let second = Node::open(
        NodeConfig::new(second_dir.path())
            .community(first.community_id(), first.member_id())
            .relay_only(),
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if second
                .snapshot()
                .await
                .unwrap()
                .messages()
                .iter()
                .any(|message| message.message().body() == "relay offline")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}
