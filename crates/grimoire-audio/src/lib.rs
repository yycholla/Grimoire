use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use cpal::{
    SampleFormat, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use grimoire_core::{ChannelId, Command, Event, Node, VoiceFrame, VoiceStreamId};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use thiserror::Error;
use tokio::{
    sync::{mpsc as tokio_mpsc, oneshot, watch},
    task::JoinHandle,
};

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960;
const MAX_PACKET_BYTES: usize = 1024;
const QUEUED_FRAMES: usize = 8;
const JITTER_FRAMES: usize = 3;
const MAX_REMOTE_STREAMS: usize = 3;
const MAX_STREAM_QUEUE: usize = 50;
const MAX_MISSED_FRAMES: usize = 10;

#[derive(Clone, Debug, Error)]
#[error("{0}")]
pub struct AudioError(String);

impl AudioError {
    fn from(error: impl fmt::Display) -> Self {
        Self(error.to_string())
    }

    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoiceDeviceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoiceDeviceNames {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

pub fn available_devices() -> Result<VoiceDeviceNames, AudioError> {
    let host = cpal::default_host();
    let mut input = host
        .input_devices()
        .map_err(AudioError::from)?
        .filter_map(|device| {
            device
                .description()
                .ok()
                .map(|description| description.name().to_owned())
        })
        .collect::<Vec<_>>();
    let mut output = host
        .output_devices()
        .map_err(AudioError::from)?
        .filter_map(|device| {
            device
                .description()
                .ok()
                .map(|description| description.name().to_owned())
        })
        .collect::<Vec<_>>();
    input.sort();
    input.dedup();
    output.sort();
    output.dedup();
    Ok(VoiceDeviceNames { input, output })
}

#[derive(Clone, Debug, PartialEq)]
struct PcmFrame([f32; FRAME_SAMPLES]);

impl PcmFrame {
    fn new(samples: Vec<f32>) -> Result<Self, AudioError> {
        samples.try_into().map(Self).map_err(|samples: Vec<f32>| {
            AudioError::message(format!(
                "expected {FRAME_SAMPLES} samples, got {}",
                samples.len()
            ))
        })
    }

    fn samples(&self) -> &[f32; FRAME_SAMPLES] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AudioStreamId([u8; 16]);

impl AudioStreamId {
    const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

struct VoiceEncoder(Encoder);

impl VoiceEncoder {
    fn new() -> Result<Self, AudioError> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .map_err(AudioError::from)?;
        encoder
            .set_bitrate(Bitrate::Bits(32_000))
            .map_err(AudioError::from)?;
        Ok(Self(encoder))
    }

    fn encode(&mut self, samples: &PcmFrame) -> Result<Vec<u8>, AudioError> {
        self.0
            .encode_vec_float(samples.samples(), MAX_PACKET_BYTES)
            .map_err(AudioError::from)
    }
}

struct VoiceDecoder(Decoder);

impl VoiceDecoder {
    fn new() -> Result<Self, AudioError> {
        Decoder::new(SAMPLE_RATE, Channels::Mono)
            .map(Self)
            .map_err(AudioError::from)
    }

    fn decode(&mut self, packet: &[u8]) -> Result<PcmFrame, AudioError> {
        let mut samples = vec![0.0; FRAME_SAMPLES];
        let decoded = self
            .0
            .decode_float(packet, &mut samples, false)
            .map_err(AudioError::from)?;
        samples.truncate(decoded);
        PcmFrame::new(samples)
    }
}

struct VoicePlayout {
    streams: HashMap<AudioStreamId, StreamState>,
}

impl VoicePlayout {
    fn new() -> Self {
        Self {
            streams: HashMap::new(),
        }
    }

    fn push(
        &mut self,
        id: AudioStreamId,
        sequence: u64,
        packet: Vec<u8>,
    ) -> Result<(), AudioError> {
        if packet.is_empty() || packet.len() > MAX_PACKET_BYTES {
            return Err(AudioError::message("invalid Opus packet size"));
        }
        if !self.streams.contains_key(&id) {
            if self.streams.len() == MAX_REMOTE_STREAMS {
                return Err(AudioError::message(
                    "voice playout already has three remote streams",
                ));
            }
            self.streams.insert(id, StreamState::new()?);
        }

        let stream = self.streams.get_mut(&id).expect("stream was inserted");
        if stream
            .next_sequence
            .is_some_and(|next_sequence| sequence < next_sequence)
            || stream.packets.len() == MAX_STREAM_QUEUE
        {
            return Ok(());
        }
        stream.packets.entry(sequence).or_insert(packet);
        stream.startup_ticks = 0;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<PcmFrame> {
        let mut mixed = [0.0; FRAME_SAMPLES];
        let mut active = 0;
        let mut expired = Vec::new();

        for (id, stream) in &mut self.streams {
            if stream.next_sequence.is_none() {
                if stream.packets.len() < JITTER_FRAMES {
                    stream.startup_ticks += 1;
                    if stream.startup_ticks > MAX_MISSED_FRAMES {
                        expired.push(*id);
                    }
                    continue;
                }
                stream.next_sequence = stream
                    .packets
                    .first_key_value()
                    .map(|(sequence, _)| *sequence);
            }
            let sequence = stream.next_sequence.expect("started stream has a sequence");
            let packet = stream.packets.remove(&sequence);
            let Ok(frame) = stream.decoder.decode(packet.as_deref().unwrap_or_default()) else {
                expired.push(*id);
                continue;
            };
            stream.next_sequence = Some(sequence.saturating_add(1));
            if packet.is_some() {
                stream.missed_frames = 0;
            } else {
                stream.missed_frames += 1;
            }
            for (mixed, sample) in mixed.iter_mut().zip(frame.samples()) {
                *mixed += sample;
            }
            active += 1;
            if stream.missed_frames > MAX_MISSED_FRAMES {
                expired.push(*id);
            }
        }
        for id in expired {
            self.streams.remove(&id);
        }

        if active == 0 {
            return None;
        }
        for sample in &mut mixed {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Some(PcmFrame(mixed))
    }
}

impl Default for VoicePlayout {
    fn default() -> Self {
        Self::new()
    }
}

pub struct VoiceSession {
    state: Arc<VoiceState>,
    mute_updates: tokio_mpsc::UnboundedSender<bool>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), AudioError>>,
    completion: VoiceCompletion,
}

#[derive(Clone)]
pub struct VoiceCompletion(watch::Receiver<Option<Result<(), AudioError>>>);

impl VoiceCompletion {
    pub async fn wait(mut self) -> Result<(), AudioError> {
        loop {
            if let Some(result) = self.0.borrow().clone() {
                return result;
            }
            self.0
                .changed()
                .await
                .map_err(|_| AudioError::message("voice completion stream closed"))?;
        }
    }
}

fn completion_channel() -> (
    watch::Sender<Option<Result<(), AudioError>>>,
    VoiceCompletion,
) {
    let (sender, receiver) = watch::channel(None);
    (sender, VoiceCompletion(receiver))
}

impl VoiceSession {
    pub async fn join(node: Arc<Node>) -> Result<Self, AudioError> {
        Self::join_channel(node, ChannelId::VOICE_ROOM).await
    }

    pub async fn join_channel(node: Arc<Node>, channel_id: ChannelId) -> Result<Self, AudioError> {
        Self::join_channel_with_config(node, channel_id, VoiceDeviceConfig::default()).await
    }

    pub async fn join_channel_with_config(
        node: Arc<Node>,
        channel_id: ChannelId,
        config: VoiceDeviceConfig,
    ) -> Result<Self, AudioError> {
        let state = Arc::new(VoiceState::default());
        let audio = VoiceAudio::open(&config, state.clone())?;
        let encoder = VoiceEncoder::new()?;
        node.execute(Command::SetVoicePresence {
            channel: channel_id,
            state: grimoire_core::VoicePresence::Joined,
        })
        .await
        .map_err(AudioError::from)?;
        let (mute_updates, mute_changes) = tokio_mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let (completed, completion) = completion_channel();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let result = run_session(
                node,
                channel_id,
                audio,
                encoder,
                task_state,
                mute_changes,
                stopped,
            )
            .await;
            let _ = completed.send(Some(result.clone()));
            result
        });
        Ok(Self {
            state,
            mute_updates,
            stop,
            task,
            completion,
        })
    }

    pub fn set_muted(&self, muted: bool) {
        self.state.set_muted(muted);
        let _ = self.mute_updates.send(muted);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.state.set_deafened(deafened);
    }

    pub fn completion(&self) -> VoiceCompletion {
        self.completion.clone()
    }

    pub async fn leave(self) -> Result<(), AudioError> {
        let _ = self.stop.send(());
        self.task.await.map_err(AudioError::from)?
    }
}

#[derive(Default)]
struct VoiceState {
    muted: AtomicBool,
    deafened: AtomicBool,
}

impl VoiceState {
    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
    }

    fn is_deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }
}

async fn run_session(
    node: Arc<Node>,
    channel_id: ChannelId,
    mut audio: VoiceAudio,
    mut encoder: VoiceEncoder,
    state: Arc<VoiceState>,
    mut mute_updates: tokio_mpsc::UnboundedReceiver<bool>,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), AudioError> {
    let mut events = node.subscribe();
    let mut playout = VoicePlayout::new();
    let mut playout_tick = tokio::time::interval(Duration::from_millis(20));
    playout_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stream_id = VoiceStreamId::from_bytes(rand::random());
    let mut sequence = 0;

    let result = async {
        loop {
            tokio::select! {
                samples = audio.next_event() => {
                    let Some(samples) = samples? else { break };
                    if state.is_muted() {
                        continue;
                    }
                    let packet = encoder.encode(&samples)?;
                    let frame = VoiceFrame::in_channel(stream_id, channel_id, sequence, packet)
                        .map_err(AudioError::from)?;
                    node.execute(Command::SendVoice(frame))
                        .await
                        .map_err(AudioError::from)?;
                    sequence += 1;
                }
                muted = mute_updates.recv() => {
                    let Some(muted) = muted else { break };
                    node.execute(Command::SetVoicePresence {
                        channel: channel_id,
                        state: grimoire_core::VoicePresence::Muted(muted),
                    })
                    .await
                    .map_err(AudioError::from)?;
                }
                result = events.recv() => match result {
                    Ok(Event::VoiceReceived(authored)) => {
                        let frame = authored.frame();
                        if !is_joined_channel(frame, channel_id) {
                            continue;
                        }
                        if let Err(error) = playout.push(
                            AudioStreamId::from_bytes(*frame.stream_id().as_bytes()),
                            frame.sequence(),
                            frame.payload().to_vec(),
                        ) {
                            eprintln!("voice frame dropped: {error}");
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = playout_tick.tick() => {
                    if let Some(frame) = suppress_playout(
                        state.is_deafened(),
                        playout.next_frame(),
                    ) {
                        audio.play(frame);
                    }
                }
                _ = &mut stopped => break,
            }
        }
        Ok(())
    }
    .await;
    let _ = node
        .execute(Command::SetVoicePresence {
            channel: channel_id,
            state: grimoire_core::VoicePresence::Left,
        })
        .await;
    result
}

fn suppress_playout(deafened: bool, frame: Option<PcmFrame>) -> Option<PcmFrame> {
    (!deafened).then_some(frame).flatten()
}

fn is_joined_channel(frame: &VoiceFrame, channel_id: ChannelId) -> bool {
    frame.channel_id() == channel_id
}

struct StreamState {
    decoder: VoiceDecoder,
    packets: BTreeMap<u64, Vec<u8>>,
    next_sequence: Option<u64>,
    missed_frames: usize,
    startup_ticks: usize,
}

impl StreamState {
    fn new() -> Result<Self, AudioError> {
        Ok(Self {
            decoder: VoiceDecoder::new()?,
            packets: BTreeMap::new(),
            next_sequence: None,
            missed_frames: 0,
            startup_ticks: 0,
        })
    }
}

struct VoiceAudio {
    captured: tokio_mpsc::Receiver<PcmFrame>,
    errors: tokio_mpsc::UnboundedReceiver<AudioError>,
    playback: mpsc::SyncSender<PcmFrame>,
    _input: cpal::Stream,
    _output: cpal::Stream,
}

impl VoiceAudio {
    fn open(config: &VoiceDeviceConfig, state: Arc<VoiceState>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let input_devices = config.input_device.as_ref().and_then(|_| {
            host.input_devices().ok().map(|devices| {
                devices
                    .filter_map(|device| {
                        device
                            .description()
                            .ok()
                            .map(|description| (description.name().to_owned(), device))
                    })
                    .collect::<Vec<_>>()
            })
        });
        let output_devices = config.output_device.as_ref().and_then(|_| {
            host.output_devices().ok().map(|devices| {
                devices
                    .filter_map(|device| {
                        device
                            .description()
                            .ok()
                            .map(|description| (description.name().to_owned(), device))
                    })
                    .collect::<Vec<_>>()
            })
        });
        let (input_device, input_config) = select_device(
            config.input_device.as_deref(),
            input_devices.as_deref().unwrap_or_default(),
            host.default_input_device(),
            input_config,
            "input",
        )?;
        let (output_device, output_config) = select_device(
            config.output_device.as_deref(),
            output_devices.as_deref().unwrap_or_default(),
            host.default_output_device(),
            output_config,
            "output",
        )?;
        let input_channels = input_config.channels() as usize;
        let output_channels = output_config.channels() as usize;
        let (captured_tx, captured) = tokio_mpsc::channel(QUEUED_FRAMES);
        let (error_tx, errors) = tokio_mpsc::unbounded_channel();
        let (playback, playback_rx) = mpsc::sync_channel::<PcmFrame>(QUEUED_FRAMES);

        let mut pending = Vec::with_capacity(FRAME_SAMPLES);
        let input_errors = error_tx.clone();
        let input = input_device
            .build_input_stream(
                input_config.config(),
                move |data: &[f32], _| {
                    for frame in data.chunks_exact(input_channels) {
                        pending.push(frame.iter().sum::<f32>() / input_channels as f32);
                        if pending.len() == FRAME_SAMPLES {
                            let samples = PcmFrame(
                                std::mem::replace(&mut pending, Vec::with_capacity(FRAME_SAMPLES))
                                    .try_into()
                                    .expect("capture frame has the fixed size"),
                            );
                            let _ = captured_tx.try_send(samples);
                        }
                    }
                },
                move |error| {
                    let _ = input_errors.send(AudioError::from(error));
                },
                None,
            )
            .map_err(AudioError::from)?;

        let mut queued = VecDeque::with_capacity(FRAME_SAMPLES * QUEUED_FRAMES);
        let output_state = state;
        let output = output_device
            .build_output_stream(
                output_config.config(),
                move |data: &mut [f32], _| {
                    if output_state.is_deafened() {
                        while playback_rx.try_recv().is_ok() {}
                        queued.clear();
                        data.fill(0.0);
                        return;
                    }
                    while let Ok(samples) = playback_rx.try_recv() {
                        queued.extend(samples.0);
                    }
                    for frame in data.chunks_mut(output_channels) {
                        frame.fill(queued.pop_front().unwrap_or(0.0));
                    }
                },
                move |error| {
                    let _ = error_tx.send(AudioError::from(error));
                },
                None,
            )
            .map_err(AudioError::from)?;

        input.play().map_err(AudioError::from)?;
        output.play().map_err(AudioError::from)?;
        Ok(Self {
            captured,
            errors,
            playback,
            _input: input,
            _output: output,
        })
    }

    async fn next_event(&mut self) -> Result<Option<PcmFrame>, AudioError> {
        tokio::select! {
            frame = self.captured.recv() => Ok(frame),
            error = self.errors.recv() => Err(error.unwrap_or_else(|| AudioError::message("audio error stream closed"))),
        }
    }

    fn play(&self, samples: PcmFrame) {
        let _ = self.playback.try_send(samples);
    }
}

fn selected_device_index(configured: Option<&str>, names: &[String]) -> Option<usize> {
    configured.and_then(|configured| names.iter().position(|name| name == configured))
}

fn select_device(
    configured: Option<&str>,
    devices: &[(String, cpal::Device)],
    default: Option<cpal::Device>,
    configure: fn(&cpal::Device) -> Result<SupportedStreamConfig, AudioError>,
    kind: &str,
) -> Result<(cpal::Device, SupportedStreamConfig), AudioError> {
    let names = devices
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if let Some(index) = selected_device_index(configured, &names) {
        let device = devices[index].1.clone();
        if let Ok(config) = configure(&device) {
            return Ok((device, config));
        }
    }
    let device = default.ok_or_else(|| {
        AudioError::message(match configured {
            Some(name) => format!("{kind} device '{name}' is unavailable and no default exists"),
            None => format!("no default {kind} device"),
        })
    })?;
    let config = configure(&device).map_err(|error| {
        AudioError::message(format!("no usable default {kind} device: {error}"))
    })?;
    Ok((device, config))
}

fn input_config(device: &cpal::Device) -> Result<SupportedStreamConfig, AudioError> {
    device
        .supported_input_configs()
        .map_err(AudioError::from)?
        .find_map(supported_f32_config)
        .ok_or_else(|| AudioError::message("input device does not support 48 kHz f32 audio"))
}

fn output_config(device: &cpal::Device) -> Result<SupportedStreamConfig, AudioError> {
    device
        .supported_output_configs()
        .map_err(AudioError::from)?
        .find_map(supported_f32_config)
        .ok_or_else(|| AudioError::message("output device does not support 48 kHz f32 audio"))
}

fn supported_f32_config(config: cpal::SupportedStreamConfigRange) -> Option<SupportedStreamConfig> {
    (config.sample_format() == SampleFormat::F32)
        .then(|| config.try_with_sample_rate(SAMPLE_RATE))
        .flatten()
}

#[cfg(test)]
mod tests {
    use grimoire_core::{ChannelId, VoiceFrame, VoiceStreamId};

    use super::{
        AudioError, AudioStreamId, FRAME_SAMPLES, PcmFrame, VoiceDecoder, VoiceEncoder,
        VoicePlayout, VoiceState, completion_channel, is_joined_channel, selected_device_index,
        suppress_playout,
    };

    #[test]
    fn opus_round_trip_produces_one_voice_frame() {
        let input = frame(440.0);
        let mut encoder = VoiceEncoder::new().unwrap();
        let mut decoder = VoiceDecoder::new().unwrap();

        let packet = encoder.encode(&input).unwrap();
        let output = decoder.decode(&packet).unwrap();

        assert!(!packet.is_empty());
        assert!(packet.len() <= 1024);
        assert!(
            output
                .samples()
                .iter()
                .map(|sample| sample.abs())
                .sum::<f32>()
                > 1.0
        );
    }

    #[test]
    fn playout_reorders_mixes_and_expires_remote_streams() {
        let mut playout = VoicePlayout::new();
        for stream in 0..3_u8 {
            let packets = encoded_packets(220.0 + stream as f32 * 110.0);
            let id = AudioStreamId::from_bytes([stream; 16]);
            for index in [1, 0, 2] {
                let (sequence, packet) = &packets[index];
                playout.push(id, *sequence, packet.clone()).unwrap();
            }
        }
        assert!(playout.next_frame().is_some());

        let mut stalled = VoicePlayout::new();
        for stream in 0..3_u8 {
            stalled
                .push(AudioStreamId::from_bytes([stream; 16]), 0, vec![1])
                .unwrap();
        }
        for _ in 0..11 {
            assert!(stalled.next_frame().is_none());
        }
        assert!(
            stalled
                .push(AudioStreamId::from_bytes([9; 16]), 0, vec![1])
                .is_ok()
        );
    }

    #[tokio::test]
    async fn voice_completion_reports_failure_once() {
        let (completed, completion) = completion_channel();
        completed
            .send(Some(Err(AudioError::message("device lost"))))
            .unwrap();

        assert_eq!(
            completion.wait().await.unwrap_err().to_string(),
            "device lost"
        );
    }

    #[test]
    fn mute_state_controls_transmission() {
        let muted = VoiceState::default();
        assert!(!muted.is_muted());
        muted.set_muted(true);
        assert!(muted.is_muted());
        muted.set_muted(false);
        assert!(!muted.is_muted());
    }

    #[test]
    fn device_name_selection_uses_exact_match_or_default() {
        let names = vec!["Built-in".to_owned(), "USB Headset".to_owned()];

        assert_eq!(selected_device_index(Some("USB Headset"), &names), Some(1));
        assert_eq!(selected_device_index(Some("usb headset"), &names), None);
        assert_eq!(selected_device_index(Some("Missing"), &names), None);
        assert_eq!(selected_device_index(None, &names), None);
    }

    #[test]
    fn deafen_suppresses_playout_frames() {
        let frame = PcmFrame([0.5; FRAME_SAMPLES]);

        assert_eq!(
            suppress_playout(false, Some(frame.clone())),
            Some(frame.clone())
        );
        assert_eq!(suppress_playout(true, Some(frame)), None);
        assert_eq!(suppress_playout(false, None), None);
    }

    #[test]
    fn voice_frames_are_scoped_to_the_joined_channel() {
        let joined = ChannelId::from_bytes([2; 32]);
        let other = ChannelId::from_bytes([3; 32]);
        let frame =
            VoiceFrame::in_channel(VoiceStreamId::from_bytes([4; 16]), joined, 0, vec![1]).unwrap();

        assert!(is_joined_channel(&frame, joined));
        assert!(!is_joined_channel(&frame, other));
    }

    fn frame(frequency: f32) -> PcmFrame {
        PcmFrame::new(
            (0..FRAME_SAMPLES)
                .map(|sample| {
                    (sample as f32 * frequency * std::f32::consts::TAU / 48_000.0).sin() * 0.25
                })
                .collect(),
        )
        .unwrap()
    }

    fn encoded_packets(frequency: f32) -> Vec<(u64, Vec<u8>)> {
        let mut encoder = VoiceEncoder::new().unwrap();
        (0..3)
            .map(|sequence| (sequence, encoder.encode(&frame(frequency)).unwrap()))
            .collect()
    }
}
