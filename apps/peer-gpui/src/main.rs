use std::{path::PathBuf, time::Duration};

use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Entity, ExternalPaths, IntoElement,
    ParentElement, PathPromptOptions, Render, Styled, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, rgba, size,
};
use peer_audio::{VoiceDeviceConfig, VoiceDeviceNames, available_devices};
use peer_core::{
    Attachment, Channel, ChannelId, ChannelKind, Command, CommunityInvite, DisplayName,
    MAX_ATTACHMENT_BYTES, MAX_VOICE_PARTICIPANTS, MemberId, MemberRole, MembershipChange,
    MessageId, PeerAddress, TextMessage,
};

use state::{
    AttachmentState, ChannelState, CommunityAccess, CommunityState, ConnectivityMode, MemberState,
    MessageState, Session, SessionUpdate, VoiceState, VoiceUpdate, connection_label, create_new,
    join_new, open_existing, recover, voice_room_available,
};

mod config;
mod state;
mod text_input;

use text_input::{Submitted, TextInput};

const BG: u32 = 0x0c1212;
const PANEL: u32 = 0x101a19;
const RAISED: u32 = 0x14201e;
const BORDER: u32 = 0x1d2c2c;
const BORDER_BRIGHT: u32 = 0x2a3a3a;
const MUTED: u32 = 0x3d5454;
const SECONDARY: u32 = 0x5a7a7a;
const TEXT: u32 = 0xa8c0c0;
const BRIGHT: u32 = 0xc0e0e0;
const GREEN: u32 = 0x4ade80;
const BLUE: u32 = 0x60a5fa;
const PURPLE: u32 = 0xc084fc;
const TEAL: u32 = 0x2dd4bf;
const YELLOW: u32 = 0xeab308;
const RED: u32 = 0xf87171;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Onboarding {
    Choice,
    Create,
    Join,
    Recover,
    Opening,
}

enum OpenAction {
    Create {
        path: PathBuf,
        name: DisplayName,
    },
    Join {
        path: PathBuf,
        name: DisplayName,
        invite: CommunityInvite,
        invite_text: String,
    },
    Recover {
        path: PathBuf,
        backup: PathBuf,
        passphrase: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Overlay {
    Settings,
    CreateChannel(ChannelKind),
    Member(MemberId),
    ConfirmRemove(MemberId),
}

struct Shell {
    config: config::AppConfig,
    config_path: PathBuf,
    community_paths: Vec<PathBuf>,
    current_path: Option<PathBuf>,
    state: Option<CommunityState>,
    session: Option<Session>,
    voice_background: Option<(PathBuf, CommunityState, Session)>,
    voice_origin: Option<PathBuf>,
    pending_voice: Option<(PathBuf, ChannelId)>,
    voice: VoiceState,
    invite: Option<String>,
    draft: Entity<TextInput>,
    onboarding: Onboarding,
    open_generation: u64,
    preview: bool,
    name_input: Entity<TextInput>,
    data_dir_input: Entity<TextInput>,
    invite_input: Entity<TextInput>,
    backup_input: Entity<TextInput>,
    passphrase_input: Entity<TextInput>,
    backup_confirmation_input: Entity<TextInput>,
    connect_input: Entity<TextInput>,
    channel_input: Entity<TextInput>,
    overlay: Option<Overlay>,
    switching_community: bool,
    audio_devices: VoiceDeviceNames,
    load_error: Option<String>,
}

impl Shell {
    fn new(
        opened: Result<Option<(CommunityState, Session)>, String>,
        community_paths: Vec<PathBuf>,
        config: config::AppConfig,
        config_path: PathBuf,
        preview: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = cx.new(|cx| TextInput::new(cx, "message this channel"));
        let name_input = {
            let name = config.default_name.clone().unwrap_or_default();
            cx.new(move |cx| TextInput::with_value(cx, "display name", name))
        };
        let data_dir_input = {
            let path = config
                .last_data_dir
                .clone()
                .unwrap_or_else(|| config::default_data_dir().to_string_lossy().into_owned());
            cx.new(move |cx| TextInput::with_value(cx, "community data directory", path))
        };
        let invite_input = {
            let invite = config.last_invite.clone().unwrap_or_default();
            cx.new(move |cx| TextInput::with_value(cx, "community invite", invite))
        };
        let backup_input = cx.new(|cx| TextInput::new(cx, "identity backup path"));
        let passphrase_input = cx.new(|cx| TextInput::password(cx, "backup passphrase"));
        let backup_confirmation_input =
            cx.new(|cx| TextInput::password(cx, "confirm backup passphrase"));
        let connect_input = cx.new(|cx| TextInput::new(cx, "peer address"));
        let channel_input = cx.new(|cx| TextInput::new(cx, "channel name"));
        let current_path = community_paths.first().cloned();
        let mut shell = match opened {
            Ok(Some((state, session))) => Self {
                config: config.clone(),
                config_path: config_path.clone(),
                community_paths,
                current_path,
                state: Some(state),
                session: Some(session),
                voice_background: None,
                voice_origin: None,
                pending_voice: None,
                voice: VoiceState::default(),
                invite: None,
                draft: draft.clone(),
                onboarding: Onboarding::Choice,
                open_generation: 0,
                preview,
                name_input: name_input.clone(),
                data_dir_input: data_dir_input.clone(),
                invite_input: invite_input.clone(),
                backup_input: backup_input.clone(),
                passphrase_input: passphrase_input.clone(),
                backup_confirmation_input: backup_confirmation_input.clone(),
                connect_input: connect_input.clone(),
                channel_input: channel_input.clone(),
                overlay: None,
                switching_community: false,
                audio_devices: VoiceDeviceNames::default(),
                load_error: None,
            },
            Ok(None) => Self {
                config: config.clone(),
                config_path: config_path.clone(),
                community_paths,
                current_path: None,
                state: None,
                session: None,
                voice_background: None,
                voice_origin: None,
                pending_voice: None,
                voice: VoiceState::default(),
                invite: None,
                draft: draft.clone(),
                onboarding: Onboarding::Choice,
                open_generation: 0,
                preview,
                name_input: name_input.clone(),
                data_dir_input: data_dir_input.clone(),
                invite_input: invite_input.clone(),
                backup_input: backup_input.clone(),
                passphrase_input: passphrase_input.clone(),
                backup_confirmation_input: backup_confirmation_input.clone(),
                connect_input: connect_input.clone(),
                channel_input: channel_input.clone(),
                overlay: None,
                switching_community: false,
                audio_devices: VoiceDeviceNames::default(),
                load_error: None,
            },
            Err(error) => Self {
                config,
                config_path,
                community_paths,
                current_path,
                state: None,
                session: None,
                voice_background: None,
                voice_origin: None,
                pending_voice: None,
                voice: VoiceState::default(),
                invite: None,
                draft: draft.clone(),
                onboarding: Onboarding::Choice,
                open_generation: 0,
                preview,
                name_input,
                data_dir_input,
                invite_input,
                backup_input,
                passphrase_input,
                backup_confirmation_input,
                connect_input,
                channel_input,
                overlay: None,
                switching_community: false,
                audio_devices: VoiceDeviceNames::default(),
                load_error: Some(error),
            },
        };
        if let Some(path) = shell.current_path.clone()
            && let Some(updates) = shell.session.as_ref().map(Session::updates)
        {
            Self::watch_session(path, updates, cx);
        }
        cx.subscribe(&draft, |this, _, _: &Submitted, cx| this.send_text(cx))
            .detach();
        cx.on_app_quit(|this, _| {
            let sessions = [
                this.session.take(),
                this.voice_background.take().map(|(_, _, session)| session),
            ];
            async move {
                for session in sessions.into_iter().flatten() {
                    let _ = session.close();
                }
            }
        })
        .detach();
        if let Some(session) = &shell.session {
            let _ = session.set_voice_devices(shell.voice_device_config());
        }
        shell.mark_selected_read();
        shell
    }

    fn voice_device_config(&self) -> VoiceDeviceConfig {
        VoiceDeviceConfig {
            input_device: self.config.voice_input_device.clone(),
            output_device: self.config.voice_output_device.clone(),
        }
    }

    fn show_onboarding(&mut self, screen: Onboarding, cx: &mut Context<Self>) {
        self.onboarding = screen;
        self.load_error = None;
        cx.notify();
    }

    fn open_from_onboarding(&mut self, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().trim().to_owned();
        let data_dir = self.data_dir_input.read(cx).value().trim().to_owned();
        let invite_text = self.invite_input.read(cx).value().trim().to_owned();
        let backup = self.backup_input.read(cx).value().trim().to_owned();
        let passphrase = self.passphrase_input.update(cx, TextInput::take);
        if data_dir.is_empty() {
            self.load_error = Some("choose a community data directory".to_owned());
            cx.notify();
            return;
        }
        let path = PathBuf::from(data_dir);
        let action = match self.onboarding {
            Onboarding::Create => DisplayName::new(name)
                .map_err(|error| error.to_string())
                .map(|name| OpenAction::Create { path, name }),
            Onboarding::Join => DisplayName::new(name)
                .map_err(|error| error.to_string())
                .and_then(|name| {
                    invite_text
                        .parse::<CommunityInvite>()
                        .map_err(|error| error.to_string())
                        .map(|invite| OpenAction::Join {
                            path,
                            name,
                            invite,
                            invite_text,
                        })
                }),
            Onboarding::Recover if backup.is_empty() => Err("choose an identity backup".to_owned()),
            Onboarding::Recover if passphrase.chars().count() < 12 => {
                Err("backup passphrase must contain at least 12 characters".to_owned())
            }
            Onboarding::Recover => Ok(OpenAction::Recover {
                path,
                backup: PathBuf::from(backup),
                passphrase,
            }),
            _ => return,
        };
        let action = match action {
            Ok(action) => action,
            Err(error) => {
                self.load_error = Some(error);
                cx.notify();
                return;
            }
        };
        let previous = self.onboarding;
        let connectivity = connectivity_mode(self.config.relay_only);
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        self.onboarding = Onboarding::Opening;
        self.load_error = None;
        let opening = cx.background_executor().spawn(async move {
            match action {
                OpenAction::Create { path, name } => create_new(path.clone(), connectivity)
                    .map(|opened| (opened, path, Some(name), None)),
                OpenAction::Join {
                    path,
                    name,
                    invite,
                    invite_text,
                } => join_new(path.clone(), invite, connectivity)
                    .map(|opened| (opened, path, Some(name), Some(invite_text))),
                OpenAction::Recover {
                    path,
                    backup,
                    passphrase,
                } => recover(path.clone(), backup, &passphrase, connectivity)
                    .map(|opened| (opened, path, None, None)),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != generation || this.onboarding != Onboarding::Opening {
                    return;
                }
                match result {
                    Ok(((state, session), path, name, invite)) => {
                        Self::watch_session(path.clone(), session.updates(), cx);
                        let profile_error = name.as_ref().and_then(|name| {
                            session.execute(Command::SetDisplayName(name.clone())).err()
                        });
                        let device_error =
                            session.set_voice_devices(this.voice_device_config()).err();
                        if !this.community_paths.contains(&path) {
                            this.community_paths.push(path.clone());
                        }
                        if let (Some(old_path), Some(old_state), Some(old_session)) = (
                            this.current_path.take(),
                            this.state.take(),
                            this.session.take(),
                        ) {
                            if this.voice_origin.as_ref() == Some(&old_path) {
                                this.voice_background = Some((old_path, old_state, old_session));
                            } else {
                                cx.background_executor()
                                    .spawn(async move { old_session.close() })
                                    .detach();
                            }
                        }
                        this.current_path = Some(path.clone());
                        this.state = Some(state);
                        this.session = Some(session);
                        this.config.last_data_dir = Some(path.to_string_lossy().into_owned());
                        this.config.last_invite = invite;
                        if let Some(name) = name {
                            this.config.default_name = Some(name.as_str().to_owned());
                        }
                        let config_error = config::save(&this.config_path, &this.config)
                            .err()
                            .map(|error| error.to_string());
                        this.load_error = profile_error.or(device_error).or(config_error);
                        this.onboarding = Onboarding::Choice;
                        this.switching_community = false;
                        this.mark_selected_read();
                        cx.notify();
                    }
                    Err(error) => {
                        this.onboarding = previous;
                        this.load_error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn open_overlay(&mut self, overlay: Overlay, cx: &mut Context<Self>) {
        self.overlay = Some(overlay);
        self.load_error = None;
        if overlay == Overlay::Settings {
            self.refresh_audio_devices(cx);
        }
        cx.notify();
    }

    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        self.overlay = None;
        self.load_error = None;
        cx.notify();
    }

    fn save_display_name(&mut self, cx: &mut Context<Self>) {
        let value = self.name_input.read(cx).value();
        let result = DisplayName::new(value)
            .map_err(|error| error.to_string())
            .and_then(|name| {
                self.session
                    .as_ref()
                    .ok_or_else(|| "no Community is open".to_owned())?
                    .execute(Command::SetDisplayName(name.clone()))?;
                self.config.default_name = Some(name.as_str().to_owned());
                config::save(&self.config_path, &self.config).map_err(|error| error.to_string())
            });
        self.load_error = result.err();
        cx.notify();
    }

    fn connect_from_settings(&mut self, cx: &mut Context<Self>) {
        let result = self
            .connect_input
            .read(cx)
            .value()
            .trim()
            .parse::<PeerAddress>()
            .map_err(|error| error.to_string())
            .and_then(|address| {
                self.session
                    .as_ref()
                    .ok_or_else(|| "no Community is open".to_owned())?
                    .connect(address)
            });
        self.load_error = result.err();
        cx.notify();
    }

    fn export_backup(&mut self, cx: &mut Context<Self>) {
        let path = self.backup_input.read(cx).value().trim().to_owned();
        let passphrase = self.passphrase_input.update(cx, TextInput::take);
        let confirmation = self.backup_confirmation_input.update(cx, TextInput::take);
        let result = if path.is_empty() {
            Err("choose an identity backup path".to_owned())
        } else if passphrase.chars().count() < 12 {
            Err("backup passphrase must contain at least 12 characters".to_owned())
        } else if passphrase != confirmation {
            Err("backup passphrases do not match".to_owned())
        } else {
            self.session
                .as_ref()
                .ok_or_else(|| "no Community is open".to_owned())
                .and_then(|session| session.export_identity(PathBuf::from(path), passphrase))
        };
        self.load_error = result.err();
        cx.notify();
    }

    fn create_channel_from_overlay(&mut self, kind: ChannelKind, cx: &mut Context<Self>) {
        let name = self.channel_input.update(cx, TextInput::take);
        let result = Channel::new(ChannelId::generate(), name, kind)
            .map(Command::CreateChannel)
            .map_err(|error| error.to_string())
            .and_then(|command| {
                self.session
                    .as_ref()
                    .ok_or_else(|| "no Community is open".to_owned())?
                    .execute(command)
            });
        if let Err(error) = result {
            self.load_error = Some(error);
        } else {
            self.overlay = None;
            self.load_error = None;
        }
        cx.notify();
    }

    fn remove_member(&mut self, member: MemberId, cx: &mut Context<Self>) {
        let result = self
            .session
            .as_ref()
            .ok_or_else(|| "no Community is open".to_owned())
            .and_then(|session| {
                session.execute(Command::ChangeMembership(MembershipChange::Remove(member)))
            });
        if let Err(error) = result {
            self.load_error = Some(error);
        } else {
            self.overlay = None;
            self.load_error = None;
        }
        cx.notify();
    }

    fn dismiss_member(&mut self, member: MemberId, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.state {
            state.dismiss_pending(member);
        }
        cx.notify();
    }

    fn switch_to_onboarding(&mut self, cx: &mut Context<Self>) {
        self.overlay = None;
        self.onboarding = Onboarding::Choice;
        self.switching_community = true;
        self.load_error = None;
        cx.notify();
    }

    fn cancel_community_switch(&mut self, cx: &mut Context<Self>) {
        self.switching_community = false;
        self.load_error = None;
        cx.notify();
    }

    fn toggle_relay_only(&mut self, cx: &mut Context<Self>) {
        self.config.relay_only = !self.config.relay_only;
        self.load_error = config::save(&self.config_path, &self.config)
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn refresh_audio_devices(&mut self, cx: &mut Context<Self>) {
        let scan = cx
            .background_executor()
            .spawn(async { available_devices().map_err(|error| error.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = scan.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(devices) => {
                        this.audio_devices = devices;
                        this.load_error = None;
                    }
                    Err(error) => this.load_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cycle_voice_input(&mut self, cx: &mut Context<Self>) {
        self.config.voice_input_device = cycle_device(
            self.config.voice_input_device.as_deref(),
            &self.audio_devices.input,
        );
        self.save_voice_devices(cx);
    }

    fn cycle_voice_output(&mut self, cx: &mut Context<Self>) {
        self.config.voice_output_device = cycle_device(
            self.config.voice_output_device.as_deref(),
            &self.audio_devices.output,
        );
        self.save_voice_devices(cx);
    }

    fn save_voice_devices(&mut self, cx: &mut Context<Self>) {
        let devices = self.voice_device_config();
        let session_error = [
            self.session.as_ref(),
            self.voice_background
                .as_ref()
                .map(|(_, _, session)| session),
        ]
        .into_iter()
        .flatten()
        .find_map(|session| session.set_voice_devices(devices.clone()).err());
        let config_error = config::save(&self.config_path, &self.config)
            .err()
            .map(|error| error.to_string());
        self.load_error = session_error.or(config_error);
        cx.notify();
    }

    fn toggle_voice_deafened(&mut self) {
        if let Some(session) = self.voice_session()
            && let Err(error) = session.set_voice_deafened(!self.voice.is_deafened())
        {
            self.load_error = Some(error);
        }
    }

    fn send_text(&mut self, cx: &mut Context<Self>) {
        let body = self.draft.update(cx, TextInput::take);
        if body.starts_with('/') {
            self.run_command(&body);
            return;
        }
        let Some(state) = &self.state else {
            return;
        };
        let channel = state.selected_channel;
        if !state
            .channels
            .iter()
            .any(|stored| stored.id == channel && stored.kind == ChannelKind::Text)
        {
            return;
        }
        let message = match TextMessage::in_channel(MessageId::generate(), channel, body) {
            Ok(message) => message,
            Err(error) => {
                self.load_error = Some(error.to_string());
                return;
            }
        };
        if let Some(session) = &self.session
            && let Err(error) = session.execute(Command::PostText(message))
        {
            self.load_error = Some(error);
        }
    }

    fn run_command(&mut self, input: &str) {
        if input == "/leave" {
            self.leave_voice();
            return;
        }
        if input == "/mute" {
            self.toggle_voice_mute();
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let mut saved_name = None;
        let result = if let Some(name) = input.strip_prefix("/name ") {
            DisplayName::new(name)
                .map(Command::SetDisplayName)
                .map_err(|error| error.to_string())
                .and_then(|command| session.execute(command))
                .map(|()| saved_name = Some(name.to_owned()))
        } else if let Some(rest) = input.strip_prefix("/channel ") {
            rest.split_once(' ')
                .ok_or_else(|| "usage: /channel text|voice name".to_owned())
                .and_then(|(kind, name)| match kind {
                    "text" => Ok((ChannelKind::Text, name)),
                    "voice" => Ok((ChannelKind::Voice, name)),
                    _ => Err("channel kind must be text or voice".to_owned()),
                })
                .and_then(|(kind, name)| {
                    Channel::new(ChannelId::generate(), name, kind)
                        .map(Command::CreateChannel)
                        .map_err(|error| error.to_string())
                })
                .and_then(|command| session.execute(command))
        } else if let Some(member) = input.strip_prefix("/admit ") {
            parse_member(member)
                .map(|member| Command::ChangeMembership(MembershipChange::Admit(member)))
                .and_then(|command| session.execute(command))
        } else if let Some(member) = input.strip_prefix("/remove ") {
            parse_member(member)
                .map(|member| Command::ChangeMembership(MembershipChange::Remove(member)))
                .and_then(|command| session.execute(command))
        } else if let Some(address) = input.strip_prefix("/connect ") {
            address
                .parse::<PeerAddress>()
                .map_err(|error| error.to_string())
                .and_then(|address| session.connect(address))
        } else if let Some(rest) = input.strip_prefix("/backup ") {
            rest.split_once(' ')
                .ok_or_else(|| "usage: /backup path passphrase".to_owned())
                .and_then(|(path, passphrase)| {
                    session.export_identity(PathBuf::from(path), passphrase.to_owned())
                })
        } else if let Some(path) = input.strip_prefix("/attach ") {
            self.state
                .as_ref()
                .ok_or_else(|| "no Community is open".to_owned())
                .and_then(|state| attachment_from_path(PathBuf::from(path), state.selected_channel))
                .and_then(|attachment| session.execute(Command::ShareAttachment(attachment)))
        } else if input == "/help" {
            Err(
                "commands: /name /channel /admit /remove /connect /backup /attach /mute /leave"
                    .to_owned(),
            )
        } else {
            Err("unknown command; use /help".to_owned())
        };
        if let Err(error) = result {
            self.load_error = Some(error);
        } else {
            self.load_error = None;
            if let Some(name) = saved_name {
                self.config.default_name = Some(name);
                if let Err(error) = config::save(&self.config_path, &self.config) {
                    self.load_error = Some(error.to_string());
                }
            }
        }
    }

    fn watch_session(path: PathBuf, updates: state::SessionUpdates, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(update) = updates.next().await {
                if this
                    .update(cx, |this, cx| {
                        this.apply_session_update(&path, update, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_session_update(
        &mut self,
        source: &PathBuf,
        update: SessionUpdate,
        cx: &mut Context<Self>,
    ) {
        let mut mark_read = false;
        let mut stop_voice = false;
        let mut refresh_speaking = false;
        match update {
            SessionUpdate::Event(peer_core::Event::Fault(error)) => {
                self.load_error = Some(error.to_string());
            }
            SessionUpdate::Event(event) => {
                refresh_speaking = matches!(&event, peer_core::Event::VoiceReceived(_));
                let event_channel = match &event {
                    peer_core::Event::TextStored(authored) => Some(authored.message().channel_id()),
                    peer_core::Event::AttachmentStored(authored) => {
                        Some(authored.attachment().channel_id())
                    }
                    _ => None,
                };
                mark_read = self.current_path.as_ref() == Some(source)
                    && event_channel == self.state.as_ref().map(|state| state.selected_channel);
                if self.current_path.as_ref() == Some(source)
                    && let Some(state) = &mut self.state
                {
                    let previous_access = state.access();
                    let local_voice_left = matches!(
                        &event,
                        peer_core::Event::VoicePresence {
                            channel,
                            member,
                            state: peer_core::VoicePresence::Left,
                        } if *member == state.local_member()
                            && self.voice.channel() == Some(*channel)
                    );
                    state.apply(event);
                    stop_voice = self.voice_origin.as_ref() == Some(source)
                        && (local_voice_left
                            || (previous_access != CommunityAccess::Removed
                                && state.access() == CommunityAccess::Removed));
                } else if let Some((path, state, _)) = &mut self.voice_background
                    && path == source
                {
                    let previous_access = state.access();
                    state.apply(event);
                    stop_voice = self.voice_origin.as_ref() == Some(source)
                        && previous_access != CommunityAccess::Removed
                        && state.access() == CommunityAccess::Removed;
                }
            }
            SessionUpdate::Snapshot(snapshot) => {
                mark_read = true;
                if self.current_path.as_ref() == Some(source)
                    && let Some(state) = &mut self.state
                {
                    let previous_access = state.access();
                    state.replace_snapshot(&snapshot);
                    stop_voice = self.voice_origin.as_ref() == Some(source)
                        && previous_access != CommunityAccess::Removed
                        && state.access() == CommunityAccess::Removed;
                } else if let Some((path, state, _)) = &mut self.voice_background
                    && path == source
                {
                    let previous_access = state.access();
                    state.replace_snapshot(&snapshot);
                    stop_voice = self.voice_origin.as_ref() == Some(source)
                        && previous_access != CommunityAccess::Removed
                        && state.access() == CommunityAccess::Removed;
                }
            }
            SessionUpdate::CommandFailed(error) => self.load_error = Some(error),
            SessionUpdate::InviteReady(invite) => {
                cx.write_to_clipboard(ClipboardItem::new_string(invite.clone()));
                self.invite = Some(invite);
                self.load_error = None;
            }
            SessionUpdate::ReadStored { channel, message } => {
                if self.current_path.as_ref() == Some(source)
                    && let Some(state) = &mut self.state
                {
                    state.record_read(channel, message);
                } else if let Some((path, state, _)) = &mut self.voice_background
                    && path == source
                {
                    state.record_read(channel, message);
                }
            }
            SessionUpdate::Diagnostics(peers) => {
                if self.current_path.as_ref() == Some(source)
                    && let Some(state) = &mut self.state
                {
                    state.apply_diagnostics(&peers);
                } else if let Some((path, state, _)) = &mut self.voice_background
                    && path == source
                {
                    state.apply_diagnostics(&peers);
                }
            }
            SessionUpdate::Voice(update) => self.apply_voice_update(source, update),
        }
        if stop_voice {
            self.leave_voice();
        }
        if refresh_speaking {
            let timer = cx.background_executor().timer(Duration::from_millis(700));
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        if mark_read && self.current_path.as_ref() == Some(source) {
            self.mark_selected_read();
        }
    }

    fn mark_selected_read(&mut self) {
        let Some(state) = &self.state else {
            return;
        };
        let channel = state.selected_channel;
        let Some(message) = state.latest_item(channel) else {
            return;
        };
        if let Some(session) = &self.session
            && let Err(error) = session.mark_read(channel, message)
        {
            self.load_error = Some(error);
        }
    }

    fn copy_invite(&mut self, cx: &mut Context<Self>) {
        if let Some(invite) = &self.invite {
            cx.write_to_clipboard(ClipboardItem::new_string(invite.clone()));
        } else if let Some(session) = &self.session
            && let Err(error) = session.request_invite()
        {
            self.load_error = Some(error);
        }
    }

    fn copy_peer_address(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &self.state
            && !state.local_address().is_empty()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(state.local_address().to_owned()));
        }
    }

    fn copy_member_id(&mut self, member: MemberId, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(member_hex(member)));
    }

    fn admit_member(&mut self, member: MemberId, cx: &mut Context<Self>) {
        self.load_error = self
            .session
            .as_ref()
            .ok_or_else(|| "no Community is open".to_owned())
            .and_then(|session| {
                session.execute(Command::ChangeMembership(MembershipChange::Admit(member)))
            })
            .err();
        cx.notify();
    }

    fn admit_availability_peer(&mut self, member: MemberId, cx: &mut Context<Self>) {
        self.load_error = self
            .session
            .as_ref()
            .ok_or_else(|| "no Community is open".to_owned())
            .and_then(|session| {
                session.execute(Command::ChangeMembership(
                    MembershipChange::AdmitAvailability(member),
                ))
            })
            .err();
        cx.notify();
    }

    fn save_attachment(&mut self, attachment: AttachmentState, cx: &mut Context<Self>) {
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let selected = cx.prompt_for_new_path(&directory, Some(&attachment.name));
        cx.spawn(async move |this, cx| {
            let result = match selected.await {
                Ok(Ok(Some(path))) => {
                    std::fs::write(path, attachment.bytes).map_err(|error| error.to_string())
                }
                Ok(Ok(None)) => return,
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.load_error = Some(error);
                } else {
                    this.load_error = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn forget_attachment(&mut self, attachment: AttachmentState) {
        if let Some(session) = &self.session
            && let Err(error) = session.execute(Command::ForgetAttachment {
                author: attachment.author_id,
                id: attachment.id,
            })
        {
            self.load_error = Some(error);
        }
    }

    fn pick_attachment(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &self.state else {
            return;
        };
        let channel = state.selected_channel;
        if !state
            .channels
            .iter()
            .any(|stored| stored.id == channel && stored.kind == ChannelKind::Text)
        {
            self.load_error = Some("attachments belong in text channels".to_owned());
            return;
        }
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Attach a file".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selected.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.load_error = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.load_error = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.share_attachment_path(path, cx);
            });
        })
        .detach();
    }

    fn share_attachment_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(state) = &self.state else {
            return;
        };
        let channel = state.selected_channel;
        if !state
            .channels
            .iter()
            .any(|stored| stored.id == channel && stored.kind == ChannelKind::Text)
        {
            self.load_error = Some("attachments belong in text channels".to_owned());
            cx.notify();
            return;
        }
        let reading = cx
            .background_executor()
            .spawn(async move { attachment_from_path(path, channel) });
        cx.spawn(async move |this, cx| {
            let attachment = reading.await;
            let _ = this.update(cx, |this, cx| {
                let result = attachment.and_then(|attachment| {
                    this.session
                        .as_ref()
                        .ok_or_else(|| "no Community is open".to_owned())?
                        .execute(Command::ShareAttachment(attachment))
                });
                this.load_error = result.err();
                cx.notify();
            });
        })
        .detach();
    }

    fn selected_channel_name(&self) -> String {
        self.state
            .as_ref()
            .and_then(|state| {
                state
                    .channels
                    .iter()
                    .find(|channel| channel.id == state.selected_channel)
            })
            .map_or_else(|| "general".to_owned(), |channel| channel.name.clone())
    }

    fn select_channel(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.state {
            let kind = state
                .channels
                .iter()
                .find(|stored| stored.id == channel)
                .map(|stored| stored.kind);
            if kind == Some(ChannelKind::Voice) {
                let members = state.voice_members(channel);
                let local_is_present = members
                    .iter()
                    .any(|(member, _)| member.id == state.local_member());
                if !voice_room_available(members.len(), local_is_present) {
                    self.load_error = Some(format!(
                        "voice room is full ({MAX_VOICE_PARTICIPANTS}/{MAX_VOICE_PARTICIPANTS})"
                    ));
                    cx.notify();
                    return;
                }
            }
            state.select_channel(channel);
            if kind == Some(ChannelKind::Voice) && self.voice.channel() != Some(channel) {
                let target = self.current_path.clone();
                if self.voice.channel().is_some() && self.voice_origin != target {
                    if let (Some(target), Some(origin)) = (target, self.voice_origin.clone()) {
                        self.pending_voice = Some((target, channel));
                        if let Some(session) = self.session_for(&origin)
                            && let Err(error) = session.leave_voice()
                        {
                            self.load_error = Some(error);
                        }
                    }
                } else if let (Some(path), Some(session)) = (target, &self.session) {
                    self.voice_origin = Some(path);
                    if let Err(error) = session.join_voice(channel) {
                        self.load_error = Some(error);
                    }
                }
            }
        }
        self.mark_selected_read();
        cx.notify();
    }

    fn session_for(&self, path: &PathBuf) -> Option<&Session> {
        if self.current_path.as_ref() == Some(path) {
            self.session.as_ref()
        } else {
            self.voice_background
                .as_ref()
                .and_then(|(stored, _, session)| (stored == path).then_some(session))
        }
    }

    fn state_for(&self, path: &PathBuf) -> Option<&CommunityState> {
        if self.current_path.as_ref() == Some(path) {
            self.state.as_ref()
        } else {
            self.voice_background
                .as_ref()
                .and_then(|(stored, state, _)| (stored == path).then_some(state))
        }
    }

    fn voice_session(&self) -> Option<&Session> {
        self.voice_origin
            .as_ref()
            .and_then(|path| self.session_for(path))
    }

    fn apply_voice_update(&mut self, source: &PathBuf, update: VoiceUpdate) {
        let ended = matches!(
            update,
            VoiceUpdate::Disconnected | VoiceUpdate::Failed { .. }
        );
        self.voice.apply(update);
        if ended && self.voice_origin.as_ref() == Some(source) {
            self.voice_origin = None;
            if let Some((path, state, session)) = self.voice_background.take() {
                if &path == source {
                    if let Err(error) = session.close() {
                        self.load_error = Some(error);
                    }
                } else {
                    self.voice_background = Some((path, state, session));
                }
            }
            if let Some((target, channel)) = self.pending_voice.take()
                && self.current_path.as_ref() == Some(&target)
                && let Some(session) = &self.session
            {
                self.voice_origin = Some(target);
                if let Err(error) = session.join_voice(channel) {
                    self.load_error = Some(error);
                }
            }
        }
    }

    fn onboarding_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let content = match self.onboarding {
            Onboarding::Choice => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(SECONDARY))
                        .child("Choose how to enter a Community"),
                )
                .child(
                    onboarding_choice(
                        "relay-mode",
                        if self.config.relay_only {
                            "transport: relay only ✓"
                        } else {
                            "transport: automatic (direct + relay)"
                        },
                        if self.config.relay_only { YELLOW } else { TEAL },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_relay_only(cx))),
                )
                .child(
                    onboarding_choice("create", "create a Community", GREEN).on_click(
                        cx.listener(|this, _, _, cx| this.show_onboarding(Onboarding::Create, cx)),
                    ),
                )
                .child(
                    onboarding_choice("join", "join with an invite", TEAL).on_click(
                        cx.listener(|this, _, _, cx| this.show_onboarding(Onboarding::Join, cx)),
                    ),
                )
                .child(
                    onboarding_choice("recover", "recover an identity", PURPLE).on_click(
                        cx.listener(|this, _, _, cx| this.show_onboarding(Onboarding::Recover, cx)),
                    ),
                )
                .children(self.switching_community.then(|| {
                    onboarding_choice("cancel-switch", "back to current Community", SECONDARY)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_community_switch(cx)))
                })),
            Onboarding::Opening => div()
                .flex()
                .flex_col()
                .gap_3()
                .py(px(24.0))
                .text_color(rgb(GREEN))
                .child("opening encrypted Community… keep this window open"),
            screen => {
                let title = match screen {
                    Onboarding::Create => "create a Community",
                    Onboarding::Join => "join a Community",
                    Onboarding::Recover => "recover an identity",
                    _ => unreachable!(),
                };
                let submit = match screen {
                    Onboarding::Create => "create",
                    Onboarding::Join => "join",
                    Onboarding::Recover => "recover",
                    _ => unreachable!(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_color(rgb(BRIGHT)).child(title))
                    .children(
                        matches!(screen, Onboarding::Create | Onboarding::Join)
                            .then(|| form_field("DISPLAY NAME", self.name_input.clone())),
                    )
                    .child(form_field("DATA DIRECTORY", self.data_dir_input.clone()))
                    .children(
                        (screen == Onboarding::Join)
                            .then(|| form_field("INVITE", self.invite_input.clone())),
                    )
                    .children(
                        (screen == Onboarding::Recover)
                            .then(|| form_field("IDENTITY BACKUP", self.backup_input.clone())),
                    )
                    .children(
                        (screen == Onboarding::Recover)
                            .then(|| form_field("PASSPHRASE", self.passphrase_input.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("onboarding-back")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .border_1()
                                    .border_color(rgb(BORDER_BRIGHT))
                                    .rounded(px(4.0))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_onboarding(Onboarding::Choice, cx)
                                    }))
                                    .child("back"),
                            )
                            .child(
                                div()
                                    .id("onboarding-submit")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .bg(rgb(GREEN))
                                    .text_color(rgb(BG))
                                    .rounded(px(4.0))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_from_onboarding(cx)),
                                    )
                                    .child(submit),
                            ),
                    )
            }
        };
        div()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .bg(rgb(BG))
            .font_family("monospace")
            .text_size(px(13.0))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p(px(24.0))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded(px(8.0))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(GREEN))
                            .child("grimoire // peer Community"),
                    )
                    .children(self.load_error.iter().cloned().map(|error| {
                        div()
                            .p(px(8.0))
                            .bg(rgb(RAISED))
                            .text_color(rgb(RED))
                            .child(error)
                    }))
                    .child(content),
            )
    }

    fn access_view(&self, access: CommunityAccess, cx: &mut Context<Self>) -> gpui::Div {
        let (title, detail, color) = match access {
            CommunityAccess::Waiting => (
                "⧖ waiting for admission",
                "Request sent — the owner must be online to admit you. You can safely close this window and reconnect with the same data directory.",
                YELLOW,
            ),
            CommunityAccess::Removed => (
                "your access ended",
                "History already stored on this device remains available in its data directory.",
                RED,
            ),
            CommunityAccess::Active => unreachable!(),
        };
        let state = self.state.as_ref().expect("access view requires state");
        div()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .bg(rgb(BG))
            .font_family("monospace")
            .text_size(px(13.0))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .w(px(560.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .p(px(24.0))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded(px(8.0))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(color))
                            .child(title),
                    )
                    .child(div().text_color(rgb(SECONDARY)).child(detail))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(MUTED))
                            .child(format!("identity {}", member_hex(state.local_member()))),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(SECONDARY))
                            .child(connection_label(state.connection())),
                    )
                    .children(
                        self.load_error.iter().cloned().map(|error| {
                            div().text_size(px(11.0)).text_color(rgb(RED)).child(error)
                        }),
                    )
                    .child(
                        settings_action("access-switch", "add or switch Community", TEAL)
                            .on_click(cx.listener(|this, _, _, cx| this.switch_to_onboarding(cx))),
                    ),
            )
    }

    fn overlay_view(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let overlay = self.overlay?;
        let content = match overlay {
            Overlay::Settings => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_color(rgb(BRIGHT)).child("self / settings"))
                .child(form_field("DISPLAY NAME", self.name_input.clone()))
                .child(
                    settings_action("save-name", "save display name", GREEN)
                        .on_click(cx.listener(|this, _, _, cx| this.save_display_name(cx))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(SECONDARY))
                                .child("local peer address"),
                        )
                        .child(
                            settings_action("copy-address", "copy", TEAL)
                                .on_click(cx.listener(|this, _, _, cx| this.copy_peer_address(cx))),
                        ),
                )
                .child(form_field("CONNECT TO PEER", self.connect_input.clone()))
                .child(
                    settings_action("connect-peer", "connect", GREEN)
                        .on_click(cx.listener(|this, _, _, cx| this.connect_from_settings(cx))),
                )
                .child(form_field("BACKUP PATH", self.backup_input.clone()))
                .child(form_field(
                    "BACKUP PASSPHRASE",
                    self.passphrase_input.clone(),
                ))
                .child(form_field(
                    "CONFIRM PASSPHRASE",
                    self.backup_confirmation_input.clone(),
                ))
                .child(
                    settings_action("export-backup", "create identity backup", PURPLE)
                        .on_click(cx.listener(|this, _, _, cx| this.export_backup(cx))),
                )
                .child(
                    settings_action("switch-community", "add or switch Community", YELLOW)
                        .on_click(cx.listener(|this, _, _, cx| this.switch_to_onboarding(cx))),
                )
                .child(
                    settings_action(
                        "relay-mode-settings",
                        if self.config.relay_only {
                            "relay-only transport on next open ✓"
                        } else {
                            "automatic transport on next open"
                        },
                        if self.config.relay_only { YELLOW } else { TEAL },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_relay_only(cx))),
                )
                .child(div().mt_2().text_color(rgb(BRIGHT)).child("voice devices"))
                .child(
                    settings_action(
                        "voice-input-device",
                        format!(
                            "input · {}",
                            self.config
                                .voice_input_device
                                .as_deref()
                                .unwrap_or("system default")
                        ),
                        TEAL,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_voice_input(cx))),
                )
                .child(
                    settings_action(
                        "voice-output-device",
                        format!(
                            "output · {}",
                            self.config
                                .voice_output_device
                                .as_deref()
                                .unwrap_or("system default")
                        ),
                        TEAL,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_voice_output(cx))),
                )
                .child(
                    settings_action("refresh-voice-devices", "refresh device list", SECONDARY)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_audio_devices(cx))),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child("device changes apply the next time you join voice"),
                ),
            Overlay::CreateChannel(kind) => {
                let kind_label = if kind == ChannelKind::Text {
                    "text"
                } else {
                    "voice"
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_color(rgb(BRIGHT))
                            .child(format!("create {kind_label} channel")),
                    )
                    .child(form_field("CHANNEL NAME", self.channel_input.clone()))
                    .child(
                        settings_action("create-channel", "create channel", GREEN).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.create_channel_from_overlay(kind, cx)
                            }),
                        ),
                    )
            }
            Overlay::Member(member_id) => {
                let member = self
                    .state
                    .as_ref()
                    .and_then(|state| state.members.iter().find(|member| member.id == member_id));
                let name = member
                    .map_or_else(|| "unknown member".to_owned(), |member| member.name.clone());
                let is_availability =
                    member.is_some_and(|member| member.role == MemberRole::Availability);
                let can_remove = member.is_some_and(|member| !member.is_local)
                    && self.state.as_ref().is_some_and(|state| {
                        state
                            .members
                            .iter()
                            .any(|member| member.is_local && member.is_owner)
                    });
                let paths = self
                    .state
                    .as_ref()
                    .map_or_else(Vec::new, |state| state.peer_paths(member_id));
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_color(rgb(BRIGHT)).child(name))
                    .children(is_availability.then(|| {
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(TEAL))
                            .child("availability peer · encrypted retention only")
                    }))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(SECONDARY))
                            .child(member_hex(member_id)),
                    )
                    .child(
                        settings_action("copy-member", "copy identity fingerprint", TEAL).on_click(
                            cx.listener(move |this, _, _, cx| this.copy_member_id(member_id, cx)),
                        ),
                    )
                    .children(paths.into_iter().map(|(kind, selected, rtt)| {
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(if selected { GREEN } else { MUTED }))
                            .child(format!(
                                "{}{}",
                                connection_label(Some((kind, rtt))),
                                if selected { " · selected" } else { "" }
                            ))
                    }))
                    .children(can_remove.then(|| {
                        settings_action("remove-member", "remove from Community", RED).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.open_overlay(Overlay::ConfirmRemove(member_id), cx)
                            }),
                        )
                    }))
            }
            Overlay::ConfirmRemove(member) => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_color(rgb(RED))
                        .child("Remove this member from the Community?"),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(SECONDARY))
                        .child(member_hex(member)),
                )
                .child(
                    settings_action("confirm-remove", "confirm removal", RED).on_click(
                        cx.listener(move |this, _, _, cx| this.remove_member(member, cx)),
                    ),
                ),
        };
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x000000cc))
                .child(
                    div()
                        .w(px(520.0))
                        .max_h(px(610.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p(px(20.0))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER_BRIGHT))
                        .rounded(px(8.0))
                        .children(self.load_error.iter().cloned().map(|error| {
                            div()
                                .p(px(8.0))
                                .bg(rgb(RAISED))
                                .text_color(rgb(RED))
                                .child(error)
                        }))
                        .child(content)
                        .child(
                            settings_action("close-overlay", "close", SECONDARY)
                                .on_click(cx.listener(|this, _, _, cx| this.close_overlay(cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn toggle_voice_mute(&mut self) {
        if let Some(session) = self.voice_session()
            && let Err(error) = session.set_voice_muted(!self.voice.is_muted())
        {
            self.load_error = Some(error);
        }
    }

    fn leave_voice(&mut self) {
        if let Some(session) = self.voice_session()
            && let Err(error) = session.leave_voice()
        {
            self.load_error = Some(error);
        }
    }

    fn switch_community(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.current_path.as_ref() == Some(&path) {
            return;
        }

        let next = if self
            .voice_background
            .as_ref()
            .is_some_and(|(stored, _, _)| stored == &path)
        {
            self.voice_background.take()
        } else {
            match open_existing(path.clone(), connectivity_mode(self.config.relay_only)) {
                Ok((state, session)) => {
                    Self::watch_session(path.clone(), session.updates(), cx);
                    Some((path.clone(), state, session))
                }
                Err(error) => {
                    self.load_error = Some(error);
                    return;
                }
            }
        };
        let Some((next_path, next_state, next_session)) = next else {
            return;
        };
        if let Err(error) = next_session.set_voice_devices(self.voice_device_config()) {
            self.load_error = Some(error);
        }

        if let (Some(old_path), Some(old_state), Some(old_session)) = (
            self.current_path.take(),
            self.state.take(),
            self.session.take(),
        ) {
            if self.voice_origin.as_ref() == Some(&old_path) {
                self.voice_background = Some((old_path, old_state, old_session));
            } else if let Err(error) = old_session.close() {
                self.load_error = Some(error);
            }
        }
        self.current_path = Some(next_path);
        self.state = Some(next_state);
        self.session = Some(next_session);
        self.invite = None;
        self.mark_selected_read();
        let previous_data_dir = self.config.last_data_dir.clone();
        self.config.last_data_dir = self
            .current_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        if self.config.last_data_dir != previous_data_dir {
            self.config.last_invite = None;
        }
        if let Err(error) = config::save(&self.config_path, &self.config) {
            self.load_error = Some(error.to_string());
        }
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let channel = self.selected_channel_name();
        let badge = if self.state.as_ref().is_some_and(|state| {
            state
                .members
                .iter()
                .any(|member| member.is_local && member.is_owner)
        }) {
            "copy invite"
        } else if self.state.is_some() {
            "connected"
        } else {
            "design preview"
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(42.0))
            .px(px(14.0))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().text_color(rgb(GREEN)).child("grimoire"))
            .child(div().text_color(rgb(MUTED)).child("//"))
            .child(div().text_color(rgb(BRIGHT)).child("egregore"))
            .child(div().text_color(rgb(MUTED)).child("/"))
            .child(div().text_color(rgb(SECONDARY)).child("#"))
            .child(div().text_color(rgb(BRIGHT)).child(channel))
            .child(div().flex_1())
            .children(
                self.load_error
                    .iter()
                    .cloned()
                    .map(|error| div().mr(px(10.0)).text_color(rgb(RED)).child(error)),
            )
            .child(
                div()
                    .id("copy-invite")
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(BORDER_BRIGHT))
                    .rounded(px(4.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .text_size(px(11.0))
                    .text_color(rgb(TEXT))
                    .hover(|style| style.border_color(rgb(GREEN)).text_color(rgb(GREEN)))
                    .on_click(cx.listener(|this, _, _, cx| this.copy_invite(cx)))
                    .child(badge),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = if let Some(state) = &self.state {
            let channels = state.channels.clone();
            let communities = self.community_paths.clone();
            div()
                .flex()
                .flex_col()
                .flex_1()
                .py(px(12.0))
                .child(section("COMMUNITIES"))
                .children(
                    communities
                        .into_iter()
                        .enumerate()
                        .map(|(index, path)| self.community_row(index, path, cx)),
                )
                .child(self.channel_section("TEXT", ChannelKind::Text, cx))
                .children(
                    channels
                        .iter()
                        .cloned()
                        .enumerate()
                        .filter(|(_, channel)| channel.kind == ChannelKind::Text)
                        .map(|(index, channel)| self.channel_row(index, channel, cx)),
                )
                .child(self.channel_section("VOICE", ChannelKind::Voice, cx))
                .children(
                    channels
                        .into_iter()
                        .enumerate()
                        .filter(|(_, channel)| channel.kind == ChannelKind::Voice)
                        .flat_map(|(index, channel)| {
                            let channel_id = channel.id;
                            let members = state.voice_members(channel_id);
                            std::iter::once(self.channel_row(index, channel, cx).into_any_element())
                                .chain(members.into_iter().map(move |(member, muted)| {
                                    let speaking = state.is_speaking(channel_id, member.id);
                                    voice_member_row(member, muted, speaking).into_any_element()
                                }))
                        }),
                )
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .py(px(12.0))
                .child(tree_row(&[("!  1 knock — review", YELLOW)], false))
                .child(section("TEXT"))
                .child(tree_row(
                    &[
                        ("├  ", MUTED),
                        ("#  ", SECONDARY),
                        ("general  ", GREEN),
                        ("◂", GREEN),
                    ],
                    true,
                ))
                .child(tree_row(
                    &[
                        ("└  ", MUTED),
                        ("#  ", SECONDARY),
                        ("ops       ", TEXT),
                        ("●2", GREEN),
                    ],
                    false,
                ))
                .child(section("VOICE"))
                .child(tree_row(
                    &[
                        ("└  ", MUTED),
                        ("~  ", SECONDARY),
                        ("lounge    ", TEXT),
                        ("2/4", MUTED),
                    ],
                    false,
                ))
                .child(voice_peer("s", "sable", "▮▮▮▮▮", true))
                .child(voice_peer("w", "wren", "mic off", false))
        };
        let local_name = self
            .state
            .as_ref()
            .and_then(|state| state.members.iter().find(|member| member.is_local))
            .map_or_else(|| "maren".to_owned(), |member| member.name.clone());
        div()
            .flex()
            .flex_col()
            .w(px(220.0))
            .h_full()
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(tree)
            .children(
                (self.state.is_none() || self.voice.channel().is_some())
                    .then(|| self.voice_dock(cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(42.0))
                    .px(px(14.0))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(12.0))
                    .child(div().flex_1().child(format!("{local_name}@egregore  ◆")))
                    .child(
                        div()
                            .id("copy-peer-address")
                            .cursor_pointer()
                            .text_color(rgb(SECONDARY))
                            .hover(|style| style.text_color(rgb(BRIGHT)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_overlay(Overlay::Settings, cx)
                            }))
                            .child("≡"),
                    ),
            )
    }

    fn channel_section(
        &self,
        label: &'static str,
        kind: ChannelKind,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let can_create = self.state.as_ref().is_some_and(|state| {
            state
                .members
                .iter()
                .any(|member| member.is_local && member.is_owner)
        });
        div()
            .flex()
            .items_center()
            .px(px(14.0))
            .pt(px(8.0))
            .text_size(px(10.0))
            .text_color(rgb(MUTED))
            .child(div().flex_1().child(label))
            .children(can_create.then(|| {
                div()
                    .id(("create-channel", usize::from(kind == ChannelKind::Voice)))
                    .cursor_pointer()
                    .px(px(4.0))
                    .text_color(rgb(SECONDARY))
                    .hover(|style| style.text_color(rgb(GREEN)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_overlay(Overlay::CreateChannel(kind), cx)
                    }))
                    .child("+")
            }))
    }

    fn channel_row(
        &self,
        index: usize,
        channel: ChannelState,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self
            .state
            .as_ref()
            .is_some_and(|state| state.selected_channel == channel.id);
        let unread = self
            .state
            .as_ref()
            .map_or(0, |state| state.unread_count(channel.id));
        let id = channel.id;
        let sigil = if channel.kind == ChannelKind::Text {
            "#"
        } else {
            "~"
        };
        let occupancy = self
            .state
            .as_ref()
            .filter(|_| channel.kind == ChannelKind::Voice)
            .map(|state| state.voice_members(channel.id).len());
        div()
            .id(("channel", index))
            .cursor_pointer()
            .h(px(24.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .bg(rgb(if active { RAISED } else { BG }))
            .text_size(px(12.5))
            .hover(|style| style.bg(rgb(PANEL)))
            .on_click(cx.listener(move |this, _, _, cx| this.select_channel(id, cx)))
            .child(div().text_color(rgb(MUTED)).child("└  "))
            .child(div().text_color(rgb(SECONDARY)).child(format!("{sigil}  ")))
            .child(
                div()
                    .flex_1()
                    .text_color(rgb(if active { GREEN } else { TEXT }))
                    .child(channel.name),
            )
            .children((unread > 0).then(|| {
                div()
                    .ml(px(5.0))
                    .text_size(px(10.0))
                    .text_color(rgb(GREEN))
                    .child(unread.to_string())
            }))
            .children(occupancy.map(|count| {
                div()
                    .ml(px(8.0))
                    .text_size(px(10.0))
                    .text_color(rgb(if count >= MAX_VOICE_PARTICIPANTS {
                        RED
                    } else {
                        MUTED
                    }))
                    .child(format!("{count}/{MAX_VOICE_PARTICIPANTS}"))
            }))
    }

    fn community_row(
        &self,
        index: usize,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.current_path.as_ref() == Some(&path);
        let voice = self.voice_origin.as_ref() == Some(&path);
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("community")
            .to_owned();
        div()
            .id(("community", index))
            .cursor_pointer()
            .h(px(24.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .bg(rgb(if active { RAISED } else { BG }))
            .text_size(px(12.5))
            .hover(|style| style.bg(rgb(PANEL)))
            .on_click(cx.listener(move |this, _, _, cx| this.switch_community(path.clone(), cx)))
            .child(div().w(px(18.0)).text_color(rgb(GREEN)).child(if voice {
                "●"
            } else {
                "○"
            }))
            .child(
                div()
                    .text_color(rgb(if active { GREEN } else { TEXT }))
                    .child(label),
            )
    }

    fn voice_dock(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let channel = self
            .voice
            .channel()
            .and_then(|id| {
                self.state_for(self.voice_origin.as_ref()?)?
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
            })
            .map_or("lounge", |channel| channel.name.as_str());
        let muted = self.voice.is_muted();
        let deafened = self.voice.is_deafened();
        let status = self.voice.status().to_owned();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .m(px(8.0))
            .p(px(8.0))
            .bg(rgb(RAISED))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(11.0))
                    .child(div().size(px(6.0)).rounded_full().bg(rgb(
                        if self.voice.is_connected() {
                            GREEN
                        } else {
                            YELLOW
                        },
                    )))
                    .child(
                        div()
                            .flex_1()
                            .text_color(rgb(GREEN))
                            .child(format!("~ {channel}")),
                    )
                    .child(div().text_color(rgb(SECONDARY)).child(status)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("mute")
                            .cursor_pointer()
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER_BRIGHT))
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(BRIGHT))
                            .on_click(cx.listener(|this, _, _, _| this.toggle_voice_mute()))
                            .child(if muted { "unmute" } else { "mute" }),
                    )
                    .child(
                        div()
                            .id("deafen")
                            .cursor_pointer()
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER_BRIGHT))
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(BRIGHT))
                            .on_click(cx.listener(|this, _, _, _| this.toggle_voice_deafened()))
                            .child(if deafened { "undeafen" } else { "deafen" }),
                    )
                    .child(
                        div()
                            .id("leave-voice")
                            .cursor_pointer()
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .text_size(px(11.0))
                            .text_color(rgb(RED))
                            .on_click(cx.listener(|this, _, _, _| this.leave_voice()))
                            .child("leave"),
                    ),
            )
    }

    fn timeline(&self, cx: &mut Context<Self>) -> gpui::Div {
        if let Some(state) = &self.state
            && state.channels.iter().any(|channel| {
                channel.id == state.selected_channel && channel.kind == ChannelKind::Voice
            })
        {
            let occupancy = state.voice_members(state.selected_channel).len();
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(rgb(SECONDARY))
                .child(
                    div()
                        .text_size(px(16.0))
                        .text_color(rgb(BRIGHT))
                        .child("voice is live in the sidebar"),
                )
                .child(format!(
                    "{occupancy}/{MAX_VOICE_PARTICIPANTS} participants · {}",
                    self.voice.status()
                ));
        }
        let content = if let Some(state) = &self.state {
            let selected = state.selected_channel;
            let mut entries = state
                .messages
                .iter()
                .filter(|message| message.channel == selected)
                .cloned()
                .map(|message| (message.id, Some(message), None))
                .chain(
                    state
                        .attachments
                        .iter()
                        .filter(|attachment| attachment.channel == selected)
                        .cloned()
                        .map(|attachment| (attachment.id, None, Some(attachment))),
                )
                .collect::<Vec<_>>();
            entries.sort_by_key(|(id, _, _)| *id);
            let mut previous_message = None;
            let mut rendered = Vec::with_capacity(entries.len());
            for (_, message, attachment) in entries {
                if let Some(message) = message {
                    let millis = message_millis(message.id);
                    let grouped = previous_message.is_some_and(|(author, previous)| {
                        author == message.author_id && millis.saturating_sub(previous) <= 300_000
                    });
                    previous_message = Some((message.author_id, millis));
                    rendered.push(real_message(message, grouped).into_any_element());
                } else {
                    previous_message = None;
                    rendered.push(
                        self.attachment_row(
                            attachment.expect("timeline entry contains one item"),
                            cx,
                        )
                        .into_any_element(),
                    );
                }
            }
            div()
                .id("timeline-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .justify_end()
                        .min_h(gpui::relative(1.0))
                        .gap_3()
                        .px(px(20.0))
                        .pt(px(16.0))
                        .pb(px(10.0))
                        .child(day_rule())
                        .children(rendered),
                )
        } else {
            div()
                .id("preview-timeline-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .justify_end()
                        .min_h(gpui::relative(1.0))
                        .gap_3()
                        .px(px(20.0))
                        .pt(px(16.0))
                        .pb(px(10.0))
                        .child(day_rule())
                        .child(message(
                            "s",
                            "sable",
                            "13:02",
                            BLUE,
                            &[
                                "pushed the playout fix — jitter buffer holds at 60ms now",
                                "run the codec suite when you get a chance",
                            ],
                        ))
                        .child(message(
                            "m",
                            "maren",
                            "13:05",
                            GREEN,
                            &["on it. relay path still selected on my end, 180ms"],
                        ))
                        .child(attachment_message())
                        .child(message(
                            "t",
                            "tam",
                            "13:18",
                            TEAL,
                            &["direct path came back after the NAT rebind"],
                        ))
                        .child(
                            div()
                                .px(px(2.0))
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .child("sys · direct path restored · 42ms"),
                        ),
                )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(content)
            .child(self.composer(cx))
    }

    fn attachment_row(
        &self,
        attachment: AttachmentState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let save = attachment.clone();
        let forget = attachment.clone();
        let key = u64::from_be_bytes(
            attachment.id.as_bytes()[..8]
                .try_into()
                .expect("message id prefix"),
        );
        div()
            .flex()
            .gap_3()
            .child(avatar(
                attachment.author.chars().next().unwrap_or('?').to_string(),
                TEAL,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(BRIGHT))
                            .child(attachment.author),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .p(px(8.0))
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded(px(6.0))
                            .child(div().text_color(rgb(SECONDARY)).child("▤"))
                            .child(div().text_color(rgb(TEXT)).child(attachment.name))
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(SECONDARY))
                                    .child(format!("{} bytes", attachment.bytes.len())),
                            )
                            .child(
                                div()
                                    .id(("save-attachment", key))
                                    .cursor_pointer()
                                    .text_size(px(10.0))
                                    .text_color(rgb(GREEN))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.save_attachment(save.clone(), cx)
                                    }))
                                    .child("save"),
                            )
                            .child(
                                div()
                                    .id(("forget-attachment", key))
                                    .cursor_pointer()
                                    .text_size(px(10.0))
                                    .text_color(rgb(RED))
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.forget_attachment(forget.clone())
                                    }))
                                    .child("forget"),
                            ),
                    ),
            )
    }

    fn roster(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(state) = &self.state {
            div()
                .flex()
                .flex_col()
                .w(px(176.0))
                .h_full()
                .py(px(12.0))
                .border_l_1()
                .border_color(rgb(BORDER))
                .child(section(format!("MEMBERS · {}", state.members.len())))
                .children(
                    state
                        .pending_members()
                        .enumerate()
                        .map(|(index, member)| join_request_row(index, member, cx)),
                )
                .children(state.members.iter().enumerate().map(|(index, member)| {
                    member_row(index, member.clone(), state.is_online(member.id), cx)
                }))
        } else {
            div()
                .flex()
                .flex_col()
                .w(px(176.0))
                .h_full()
                .py(px(12.0))
                .border_l_1()
                .border_color(rgb(BORDER))
                .child(section("AWAKE · 4"))
                .child(roster_row(&[
                    ("├  ", MUTED),
                    ("●  ", GREEN),
                    ("maren  ", TEXT),
                    ("◆", MUTED),
                ]))
                .child(roster_row(&[
                    ("├  ", MUTED),
                    ("●  ", GREEN),
                    ("sable", TEXT),
                ]))
                .child(roster_row(&[
                    ("├  ", MUTED),
                    ("●  ", GREEN),
                    ("wren", TEXT),
                ]))
                .child(roster_row(&[("└  ", MUTED), ("●  ", GREEN), ("tam", TEXT)]))
                .child(section("DORMANT · 1"))
                .child(roster_row(&[("└  ○  ", MUTED), ("vesper", SECONDARY)]))
        }
    }

    fn status_bar(&self) -> impl IntoElement {
        if let Some(state) = &self.state {
            let connection = state.connection();
            let label = connection_label(connection);
            let online = state.online_count();
            let badge = if connection.is_some() { GREEN } else { MUTED };
            div()
                .flex()
                .items_center()
                .h(px(24.0))
                .px(px(12.0))
                .bg(rgb(RAISED))
                .text_size(px(10.5))
                .child(
                    div()
                        .mr(px(10.0))
                        .px(px(8.0))
                        .py(px(1.0))
                        .bg(rgb(badge))
                        .text_color(rgb(BG))
                        .child(label),
                )
                .child(status("e2e sealed"))
                .child(separator())
                .child(status(format!("{online}/{} awake", state.members.len())))
                .child(div().flex_1())
                .child(div().text_color(rgb(MUTED)).child("live diagnostics"))
        } else {
            div()
                .flex()
                .items_center()
                .h(px(24.0))
                .px(px(12.0))
                .bg(rgb(RAISED))
                .text_size(px(10.5))
                .child(
                    div()
                        .mr(px(10.0))
                        .px(px(8.0))
                        .py(px(1.0))
                        .bg(rgb(MUTED))
                        .text_color(rgb(BG))
                        .child("DESIGN PREVIEW"),
                )
                .child(status("representative local state"))
                .child(div().flex_1())
                .child(div().text_color(rgb(MUTED)).child("no Community open"))
        }
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().px(px(20.0)).pb(px(12.0)).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(13.0))
                .py(px(9.0))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .rounded(px(8.0))
                .child(div().text_color(rgb(GREEN)).child("›"))
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(TEXT))
                        .child(self.draft.clone()),
                )
                .child(div().text_color(rgb(SECONDARY)).child("enter to send"))
                .child(
                    div()
                        .id("attach-file")
                        .cursor_pointer()
                        .px(px(5.0))
                        .text_color(rgb(SECONDARY))
                        .hover(|style| style.text_color(rgb(BRIGHT)))
                        .on_click(cx.listener(|this, _, _, cx| this.pick_attachment(cx)))
                        .child("⊕"),
                ),
        )
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if (self.state.is_none() || self.switching_community) && !self.preview {
            return self.onboarding_view(cx).into_any_element();
        }
        if !self.preview
            && let Some(access) = self.state.as_ref().map(CommunityState::access)
            && access != CommunityAccess::Active
        {
            return self.access_view(access, cx).into_any_element();
        }
        div()
            .id("community-root")
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                for path in paths.paths() {
                    this.share_attachment_path(path.clone(), cx);
                }
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BG))
            .font_family("monospace")
            .text_size(px(13.0))
            .text_color(rgb(TEXT))
            .child(self.header(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(self.timeline(cx))
                    .child(self.roster(cx)),
            )
            .child(self.status_bar())
            .children(self.overlay_view(cx))
            .into_any_element()
    }
}

fn section(label: impl Into<gpui::SharedString>) -> impl IntoElement {
    div()
        .px(px(14.0))
        .pt(px(8.0))
        .text_size(px(10.0))
        .text_color(rgb(MUTED))
        .child(label.into())
}

fn onboarding_choice(
    id: &'static str,
    label: &'static str,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(14.0))
        .py(px(12.0))
        .border_1()
        .border_color(rgb(BORDER_BRIGHT))
        .rounded(px(6.0))
        .text_color(rgb(color))
        .hover(|style| style.bg(rgb(RAISED)).border_color(rgb(color)))
        .child(label)
}

fn settings_action(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(10.0))
        .py(px(7.0))
        .border_1()
        .border_color(rgb(BORDER_BRIGHT))
        .rounded(px(4.0))
        .text_color(rgb(color))
        .hover(|style| style.border_color(rgb(color)).bg(rgb(RAISED)))
        .child(label.into())
}

fn cycle_device(current: Option<&str>, devices: &[String]) -> Option<String> {
    match current {
        None => devices.first().cloned(),
        Some(current) => devices
            .iter()
            .position(|device| device == current)
            .and_then(|index| devices.get(index + 1))
            .cloned(),
    }
}

fn form_field(label: &'static str, input: Entity<TextInput>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .child(label),
        )
        .child(
            div()
                .px(px(10.0))
                .py(px(8.0))
                .bg(rgb(BG))
                .border_1()
                .border_color(rgb(BORDER_BRIGHT))
                .rounded(px(4.0))
                .child(input),
        )
}

fn tree_row(parts: &'static [(&'static str, u32)], active: bool) -> impl IntoElement {
    div()
        .h(px(24.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .bg(rgb(if active { RAISED } else { BG }))
        .text_size(px(12.5))
        .hover(|style| style.bg(rgb(PANEL)))
        .children(
            parts
                .iter()
                .map(|(label, color)| div().text_color(rgb(*color)).child(*label)),
        )
}

fn voice_peer(
    initial: &'static str,
    name: &'static str,
    state: &'static str,
    speaking: bool,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(26.0))
        .pl(px(30.0))
        .pr(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(18.0))
                .rounded_full()
                .bg(rgb(RAISED))
                .border_1()
                .border_color(rgb(if speaking { GREEN } else { BORDER_BRIGHT }))
                .text_size(px(10.0))
                .text_color(rgb(if speaking { GREEN } else { SECONDARY }))
                .child(initial),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(11.0))
                .text_color(rgb(if speaking { GREEN } else { SECONDARY }))
                .child(name),
        )
        .child(
            div()
                .text_size(px(9.0))
                .text_color(rgb(if speaking { GREEN } else { MUTED }))
                .child(state),
        )
}

fn voice_member_row(member: MemberState, muted: bool, speaking: bool) -> impl IntoElement {
    let initial = member.name.chars().next().unwrap_or('?').to_string();
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(26.0))
        .pl(px(30.0))
        .pr(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(18.0))
                .rounded_full()
                .bg(rgb(RAISED))
                .border_1()
                .border_color(rgb(if speaking { GREEN } else { BORDER_BRIGHT }))
                .text_size(px(10.0))
                .text_color(rgb(if speaking { GREEN } else { SECONDARY }))
                .child(initial),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(11.0))
                .text_color(rgb(SECONDARY))
                .child(member.name),
        )
        .children(muted.then(|| {
            div()
                .text_size(px(9.0))
                .text_color(rgb(MUTED))
                .child("mic off")
        }))
        .children((speaking && !muted).then(|| {
            div()
                .text_size(px(9.0))
                .text_color(rgb(GREEN))
                .child("speaking")
        }))
}

fn day_rule() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().h(px(1.0)).flex_1().bg(rgb(BORDER)))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .child("T O D A Y"),
        )
        .child(div().h(px(1.0)).flex_1().bg(rgb(BORDER)))
}

fn real_message(message: MessageState, grouped: bool) -> impl IntoElement {
    let color = member_color(message.author_id);
    let initial = message.author.chars().next().unwrap_or('?').to_string();
    let time = message_time(message.id);
    div()
        .flex()
        .gap_3()
        .mx(px(-8.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .hover(|style| style.bg(rgb(PANEL)))
        .child(if grouped {
            div().w(px(28.0)).into_any_element()
        } else {
            avatar(initial, color).into_any_element()
        })
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .children((!grouped).then(|| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_color(rgb(color)).child(message.author))
                        .child(div().text_size(px(10.0)).text_color(rgb(MUTED)).child(time))
                }))
                .child(div().text_color(rgb(TEXT)).child(message.body)),
        )
}

fn member_color(member: peer_core::MemberId) -> u32 {
    const COLORS: [u32; 4] = [BLUE, GREEN, PURPLE, TEAL];
    let index = member
        .as_bytes()
        .iter()
        .map(|byte| usize::from(*byte))
        .sum::<usize>()
        % COLORS.len();
    COLORS[index]
}

fn message_time(id: peer_core::MessageId) -> String {
    let millis = message_millis(id);
    let minutes = millis / 60_000;
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

fn message_millis(id: peer_core::MessageId) -> u64 {
    u64::from_be_bytes(id.as_bytes()[..8].try_into().expect("message id prefix"))
}

fn message(
    initial: &'static str,
    name: &'static str,
    time: &'static str,
    color: u32,
    lines: &'static [&'static str],
) -> impl IntoElement {
    div()
        .flex()
        .gap_3()
        .mx(px(-8.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .hover(|style| style.bg(rgb(PANEL)))
        .child(avatar(initial, color))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_color(rgb(color)).child(name))
                        .child(div().text_size(px(10.0)).text_color(rgb(MUTED)).child(time)),
                )
                .children(
                    lines
                        .iter()
                        .map(|line| div().text_color(rgb(TEXT)).child(*line)),
                ),
        )
}

fn attachment_message() -> impl IntoElement {
    div()
        .flex()
        .gap_3()
        .mx(px(-8.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .hover(|style| style.bg(rgb(PANEL)))
        .child(avatar("w", PURPLE))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_color(rgb(PURPLE)).child("wren"))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(MUTED))
                                .child("13:11"),
                        ),
                )
                .child("meeting notes from this morning")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px(px(12.0))
                        .py(px(7.0))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded(px(6.0))
                        .child(div().text_color(rgb(SECONDARY)).child("▤"))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_color(rgb(BRIGHT)).child("notes-2026-07-12.md"))
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(SECONDARY))
                                        .child("1.5 KiB · held by 3 peers"),
                                ),
                        )
                        .child(action("save", GREEN, BG))
                        .child(action("forget", BORDER_BRIGHT, SECONDARY)),
                ),
        )
}

fn avatar(initial: impl Into<gpui::SharedString>, color: u32) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(30.0))
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(BORDER_BRIGHT))
        .rounded(px(8.0))
        .text_size(px(12.0))
        .text_color(rgb(color))
        .child(initial.into())
}

fn action(label: &'static str, background: u32, foreground: u32) -> impl IntoElement {
    div()
        .px(px(9.0))
        .py(px(3.0))
        .bg(rgb(background))
        .rounded(px(4.0))
        .text_size(px(11.0))
        .text_color(rgb(foreground))
        .child(label)
}

fn roster_row(parts: &'static [(&'static str, u32)]) -> impl IntoElement {
    div()
        .h(px(24.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .hover(|style| style.bg(rgb(PANEL)))
        .children(
            parts
                .iter()
                .map(|(label, color)| div().text_color(rgb(*color)).child(*label)),
        )
}

fn member_row(
    index: usize,
    member: MemberState,
    online: bool,
    cx: &mut Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let presence = if online { "●  " } else { "○  " };
    let member_id = member.id;
    div()
        .id(("member", index))
        .cursor_pointer()
        .h(px(24.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .hover(|style| style.bg(rgb(PANEL)))
        .on_click(
            cx.listener(move |this, _, _, cx| this.open_overlay(Overlay::Member(member_id), cx)),
        )
        .child(div().text_color(rgb(MUTED)).child("└  "))
        .child(
            div()
                .text_color(rgb(if online { GREEN } else { MUTED }))
                .child(presence),
        )
        .child(div().text_color(rgb(TEXT)).child(member.name))
        .children((member.role == MemberRole::Availability).then(|| {
            div()
                .ml(px(5.0))
                .text_size(px(9.0))
                .text_color(rgb(TEAL))
                .child("availability")
        }))
        .children(
            member
                .is_owner
                .then(|| div().ml(px(5.0)).text_color(rgb(MUTED)).child("◆")),
        )
}

fn join_request_row(index: usize, member: MemberId, cx: &mut Context<Shell>) -> gpui::Div {
    let label = member_hex(member);
    div()
        .h(px(48.0))
        .px(px(14.0))
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .text_size(px(10.0))
        .child(
            div()
                .text_color(rgb(YELLOW))
                .child(format!("{}…", &label[..12])),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .id(("admit", index))
                        .cursor_pointer()
                        .text_color(rgb(GREEN))
                        .hover(|style| style.text_color(rgb(BRIGHT)))
                        .on_click(cx.listener(move |this, _, _, cx| this.admit_member(member, cx)))
                        .child("admit member"),
                )
                .child(
                    div()
                        .id(("admit-availability", index))
                        .cursor_pointer()
                        .text_color(rgb(TEAL))
                        .hover(|style| style.text_color(rgb(BRIGHT)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.admit_availability_peer(member, cx)
                        }))
                        .child("admit availability"),
                )
                .child(
                    div()
                        .id(("dismiss", index))
                        .cursor_pointer()
                        .text_color(rgb(MUTED))
                        .hover(|style| style.text_color(rgb(BRIGHT)))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.dismiss_member(member, cx)),
                        )
                        .child("×"),
                ),
        )
}

fn member_hex(member: MemberId) -> String {
    member
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn status(label: impl Into<gpui::SharedString>) -> impl IntoElement {
    div().text_color(rgb(SECONDARY)).child(label.into())
}

fn separator() -> impl IntoElement {
    div().mx(px(10.0)).text_color(rgb(BORDER)).child("│")
}

fn parse_member(value: &str) -> Result<MemberId, String> {
    parse_hex_32(value).map(MemberId::from_bytes)
}

fn parse_hex_32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("identity must contain 64 hexadecimal characters".to_owned());
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "identity contains invalid hexadecimal".to_owned())?;
    }
    Ok(bytes)
}

fn attachment_from_path(path: PathBuf, channel: ChannelId) -> Result<Attachment, String> {
    let metadata = std::fs::metadata(&path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err("attachment must be a regular file up to 8 MiB".to_owned());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "attachment name is invalid".to_owned())?;
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    Attachment::new(MessageId::generate(), channel, name, bytes).map_err(|error| error.to_string())
}

const fn connectivity_mode(relay_only: bool) -> ConnectivityMode {
    if relay_only {
        ConnectivityMode::RelayOnly
    } else {
        ConnectivityMode::Wan
    }
}

fn main() {
    let config_path = config::default_path();
    let mut app_config = config::load(&config_path);
    let connectivity = connectivity_mode(app_config.relay_only);
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (community_paths, opened, stored_invite, preview) = match args.as_slice() {
        [flag, path] if flag == "--create" => {
            let path = PathBuf::from(path);
            (
                vec![path.clone()],
                create_new(path, connectivity).map(Some),
                Some(None),
                false,
            )
        }
        [flag, path, invite] if flag == "--join" => {
            let path = PathBuf::from(path);
            let opened = invite
                .parse::<CommunityInvite>()
                .map_err(|error| error.to_string())
                .and_then(|invite| join_new(path.clone(), invite, connectivity))
                .map(Some);
            (vec![path], opened, Some(Some(invite.clone())), false)
        }
        [flag] if flag == "--preview" => (Vec::new(), Ok(None), None, true),
        [] => {
            let paths = app_config
                .last_data_dir
                .as_ref()
                .map(PathBuf::from)
                .into_iter()
                .collect::<Vec<_>>();
            let opened = paths
                .first()
                .cloned()
                .map(|path| open_existing(path, connectivity))
                .transpose();
            (paths, opened, None, false)
        }
        paths => {
            let paths = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
            let opened = paths
                .first()
                .cloned()
                .map(|path| open_existing(path, connectivity))
                .transpose();
            (paths, opened, Some(None), false)
        }
    };
    if let Ok(Some((_, session))) = &opened
        && args.is_empty()
        && let Some(invite) = app_config
            .last_invite
            .as_deref()
            .and_then(|invite| invite.parse::<CommunityInvite>().ok())
    {
        let _ = session.connect(invite.owner_address().clone());
    }
    if opened.as_ref().is_ok_and(Option::is_some) {
        app_config.last_data_dir = community_paths
            .first()
            .map(|path| path.to_string_lossy().into_owned());
        if let Some(invite) = stored_invite {
            app_config.last_invite = invite;
        }
        let _ = config::save(&config_path, &app_config);
    }
    Application::new().run(move |cx: &mut App| {
        text_input::init(cx);
        let bounds = Bounds::centered(None, size(px(1080.0), px(660.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |cx| {
                    Shell::new(
                        opened,
                        community_paths,
                        app_config,
                        config_path,
                        preview,
                        cx,
                    )
                })
            },
        )
        .expect("open GPUI window");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{ConnectivityMode, connectivity_mode, cycle_device};

    #[test]
    fn connectivity_choice_selects_relay_only() {
        assert_eq!(connectivity_mode(false), ConnectivityMode::Wan);
        assert_eq!(connectivity_mode(true), ConnectivityMode::RelayOnly);
    }

    #[test]
    fn device_selection_cycles_through_defaults() {
        let devices = vec!["mic one".to_owned(), "mic two".to_owned()];

        assert_eq!(cycle_device(None, &devices).as_deref(), Some("mic one"));
        assert_eq!(
            cycle_device(Some("mic one"), &devices).as_deref(),
            Some("mic two")
        );
        assert_eq!(cycle_device(Some("mic two"), &devices), None);
        assert_eq!(cycle_device(Some("missing"), &devices), None);
    }
}
