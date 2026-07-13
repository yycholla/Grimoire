use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::{Parser, Subcommand};
use peer_audio::VoiceSession;
use peer_core::{Command, CommunityInvite, Event, MessageId, Node, NodeConfig, TextMessage};

#[derive(Debug, Parser)]
#[command(about = "Temporary peer-core connectivity harness")]
struct Args {
    #[arg(
        long,
        global = true,
        help = "Disable direct IP paths and force relay transport"
    )]
    relay_only: bool,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Serve {
        #[arg(long)]
        data_dir: PathBuf,
    },
    Send {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        address: String,
        #[arg(long)]
        body: String,
    },
    Voice {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        address: Vec<String>,
    },
    Diagnose {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        address: String,
        #[arg(long, default_value_t = 3)]
        wait_seconds: u64,
    },
    /// Retain encrypted Community data without exposing its contents.
    Availability {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        invite: CommunityInvite,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        CliCommand::Serve { data_dir } => serve(data_dir, args.relay_only).await,
        CliCommand::Send {
            data_dir,
            address,
            body,
        } => send(data_dir, &address, body, args.relay_only).await,
        CliCommand::Voice { data_dir, address } => voice(data_dir, &address, args.relay_only).await,
        CliCommand::Diagnose {
            data_dir,
            address,
            wait_seconds,
        } => diagnose(data_dir, &address, wait_seconds, args.relay_only).await,
        CliCommand::Availability { data_dir, invite } => {
            availability(data_dir, invite, args.relay_only).await
        }
    }
}

async fn availability(
    data_dir: PathBuf,
    invite: CommunityInvite,
    relay_only: bool,
) -> anyhow::Result<()> {
    let owner = invite.owner_address().member_id();
    let mut config = NodeConfig::new(data_dir).community(invite.community_id(), owner);
    config = if relay_only {
        config.relay_only()
    } else {
        config.wan()
    };
    let node = Node::open(config).await?;
    println!("identity={}", member_hex(node.member_id()));
    println!("address={}", node.address());
    println!(
        "status=waiting for owner admission; once admitted, retaining encrypted Community ciphertext without content keys"
    );

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = node.connect(invite.owner_address().clone()) => match result {
                Ok(()) => break,
                Err(_) => eprintln!("status=owner unavailable; retrying connection"),
            },
            result = &mut shutdown => {
                result?;
                node.shutdown().await?;
                return Ok(());
            }
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            result = &mut shutdown => {
                result?;
                node.shutdown().await?;
                return Ok(());
            }
        }
    }
    println!("status=connected to owner; waiting for admission or retaining encrypted ciphertext");
    shutdown.await?;
    node.shutdown().await?;
    Ok(())
}

fn member_hex(member: peer_core::MemberId) -> String {
    member
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn node_config(data_dir: PathBuf, relay_only: bool) -> NodeConfig {
    if relay_only {
        NodeConfig::new(data_dir).relay_only()
    } else {
        NodeConfig::new(data_dir).wan()
    }
}

async fn voice(data_dir: PathBuf, addresses: &[String], relay_only: bool) -> anyhow::Result<()> {
    let node = Arc::new(Node::open(node_config(data_dir, relay_only)).await?);
    println!("{}", node.address());
    for address in addresses {
        node.connect(address.parse().context("invalid peer address")?)
            .await?;
    }

    let voice = VoiceSession::join(node.clone()).await?;
    tokio::signal::ctrl_c().await?;
    voice.leave().await?;
    Arc::into_inner(node)
        .context("voice session still owns the node")?
        .shutdown()
        .await?;
    Ok(())
}

async fn serve(data_dir: PathBuf, relay_only: bool) -> anyhow::Result<()> {
    let node = Node::open(node_config(data_dir, relay_only)).await?;
    let mut events = node.subscribe();
    println!("{}", node.address());

    loop {
        tokio::select! {
            result = events.recv() => match result? {
                Event::TextStored(authored) => println!("{}", authored.message().body()),
                Event::AttachmentStored(authored) => println!("attachment received: {}", authored.attachment().name()),
                Event::AttachmentForgotten { .. } => println!("attachment forgotten locally"),
                Event::VoiceReceived(_) => println!("voice frame received"),
                Event::VoicePresence { channel, member, state } => {
                    println!("voice presence: {member:?} in {channel:?} = {state:?}")
                }
                Event::ChannelCreated(channel) => println!("channel created: {}", channel.name()),
                Event::MembershipChanged(_) => println!("community membership changed"),
                Event::DisplayNameChanged { member, name } => {
                    println!("display name changed: {member:?} = {}", name.as_str())
                }
                Event::PeerConnected(member) => println!("peer connected: {member:?}"),
                Event::Fault(error) => eprintln!("peer operation rejected: {error}"),
            },
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
        }
    }

    node.shutdown().await?;
    Ok(())
}

async fn send(
    data_dir: PathBuf,
    address: &str,
    body: String,
    relay_only: bool,
) -> anyhow::Result<()> {
    let node = Node::open(node_config(data_dir, relay_only)).await?;
    let address = address.parse().context("invalid peer address")?;
    node.connect(address).await?;
    let id = MessageId::generate();
    node.execute(Command::PostText(TextMessage::new(id, body)?))
        .await?;
    node.shutdown().await?;
    Ok(())
}

async fn diagnose(
    data_dir: PathBuf,
    address: &str,
    wait_seconds: u64,
    relay_only: bool,
) -> anyhow::Result<()> {
    let node = Node::open(node_config(data_dir, relay_only)).await?;
    node.connect(address.parse().context("invalid peer address")?)
        .await?;
    tokio::time::sleep(std::time::Duration::from_secs(wait_seconds)).await;
    for peer in node.connection_diagnostics().await {
        for path in peer.paths() {
            println!(
                "member={:?} kind={:?} selected={} rtt_ms={}",
                peer.member(),
                path.kind(),
                path.is_selected(),
                path.rtt().as_millis()
            );
        }
    }
    node.shutdown().await?;
    Ok(())
}
