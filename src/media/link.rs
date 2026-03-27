use crate::media::audio_frame_timing;
use crate::media::audio_source::AudioSourceManager;
use crate::media::negotiate::CodecInfo;
use crate::media::recorder::{Leg, Recorder};
use crate::media::transcoder::{Transcoder, rewrite_dtmf_duration};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use bytes::Bytes;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaKind as TrackMediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use rustrtc::{MediaKind, PeerConnection, RtpSender};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Duration, Interval, MissedTickBehavior};

#[derive(Clone, Debug)]
pub struct AudioMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
    pub source_codec: CodecType,
    pub target_codec: CodecType,
}

#[derive(Clone, Debug)]
pub struct DtmfMapping {
    pub source_pt: u8,
    pub target_pt: Option<u8>,
    pub source_clock_rate: u32,
    pub target_clock_rate: Option<u32>,
}

#[async_trait]
pub trait MediaSource: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}

pub struct DirectedLink {
    source: Box<dyn MediaSource>,
}

impl DirectedLink {
    pub fn new(source: Box<dyn MediaSource>) -> Self {
        Self { source }
    }

    pub fn idle() -> Self {
        Self::new(Box::new(IdleSource::new()))
    }

    pub async fn recv(&mut self) -> MediaResult<MediaSample> {
        self.source.recv().await
    }
}

#[derive(Clone)]
struct RecorderBinding {
    recorder: Arc<StdMutex<Option<Recorder>>>,
    leg: Leg,
    dtmf_pt: Option<u8>,
    dtmf_clock_rate: Option<u32>,
    codec_hint: Option<CodecType>,
}

pub struct PeerSource {
    inner: Arc<dyn MediaStreamTrack>,
    audio_mapping: Option<AudioMapping>,
    dtmf_mapping: Option<DtmfMapping>,
    transcoder: Option<Transcoder>,
    recorder: Option<RecorderBinding>,
}

impl PeerSource {
    pub fn new(
        inner: Arc<dyn MediaStreamTrack>,
        audio_mapping: Option<AudioMapping>,
        dtmf_mapping: Option<DtmfMapping>,
        transcode: Option<TranscodeSpec>,
        recorder: Option<Arc<StdMutex<Option<Recorder>>>>,
        leg: Option<Leg>,
    ) -> Self {
        let transcoder = transcode.map(|spec| {
            Transcoder::new(spec.source_codec, spec.target_codec, spec.target_pt)
        });
        let recorder = recorder.zip(leg).map(|(recorder, leg)| RecorderBinding {
            recorder,
            leg,
            dtmf_pt: dtmf_mapping.as_ref().map(|mapping| mapping.source_pt),
            dtmf_clock_rate: dtmf_mapping.as_ref().map(|mapping| mapping.source_clock_rate),
            codec_hint: audio_mapping.as_ref().map(|mapping| mapping.source_codec),
        });
        Self {
            inner,
            audio_mapping,
            dtmf_mapping,
            transcoder,
            recorder,
        }
    }

    fn record_original(&self, sample: &MediaSample) {
        let Some(binding) = self.recorder.as_ref() else {
            return;
        };
        if let Ok(mut guard) = binding.recorder.lock() {
            if let Some(recorder) = guard.as_mut() {
                let _ = recorder.write_sample(
                    binding.leg,
                    sample,
                    binding.dtmf_pt,
                    binding.dtmf_clock_rate,
                    binding.codec_hint,
                );
            }
        }
    }
}

#[async_trait]
impl MediaSource for PeerSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        loop {
            let sample = self.inner.recv().await?;
            self.record_original(&sample);

            let mut sample = sample;
            let MediaSample::Audio(frame) = &mut sample else {
                return Ok(sample);
            };

            if let Some(mapping) = self.dtmf_mapping.as_ref() {
                if frame.payload_type == Some(mapping.source_pt) {
                    let Some(target_pt) = mapping.target_pt else {
                        continue;
                    };
                    frame.payload_type = Some(target_pt);
                    if let Some(target_rate) = mapping.target_clock_rate {
                        if mapping.source_clock_rate != target_rate {
                            frame.data = rewrite_dtmf_duration(
                                &frame.data,
                                mapping.source_clock_rate,
                                target_rate,
                            );
                        }
                        frame.clock_rate = target_rate;
                    }
                    return Ok(sample);
                }
            }

            if let Some(mapping) = self.audio_mapping.as_ref() {
                if frame.payload_type != Some(mapping.source_pt) {
                    continue;
                }

                if let Some(transcoder) = self.transcoder.as_mut() {
                    let output = transcoder.transcode(frame);
                    return Ok(MediaSample::Audio(output));
                }

                frame.payload_type = Some(mapping.target_pt);
                frame.clock_rate = mapping.target_clock_rate;
                return Ok(sample);
            }

            if self.dtmf_mapping.is_none() {
                return Ok(sample);
            }
        }
    }
}

pub struct IdleSource;

impl IdleSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MediaSource for IdleSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        std::future::pending::<MediaResult<MediaSample>>().await
    }
}

pub struct FileSource {
    audio_source_manager: Arc<AudioSourceManager>,
    encoder: Box<dyn audio_codec::Encoder>,
    codec_info: CodecInfo,
    pcm_buf: Vec<i16>,
    rtp_ticks_per_frame: u32,
    ticker: Interval,
    sequence_number: u16,
    rtp_timestamp: u32,
    loop_playback: bool,
    completion_notify: Arc<Notify>,
    completion_fired: bool,
    finished: bool,
}

impl FileSource {
    pub fn new(
        file_path: String,
        loop_playback: bool,
        codec_info: CodecInfo,
        completion_notify: Arc<Notify>,
    ) -> Result<Self> {
        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !std::path::Path::new(&file_path).exists() {
            return Err(anyhow!("Audio file not found: {}", file_path));
        }

        let timing = audio_frame_timing(codec_info.codec, codec_info.clock_rate);
        let audio_source_manager = Arc::new(AudioSourceManager::new(timing.pcm_sample_rate));
        audio_source_manager.switch_to_file(file_path, loop_playback)?;
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        Ok(Self {
            audio_source_manager,
            encoder: audio_codec::create_encoder(codec_info.codec),
            codec_info,
            pcm_buf: vec![0i16; timing.pcm_samples_per_frame],
            rtp_ticks_per_frame: timing.rtp_ticks_per_frame,
            ticker,
            sequence_number: rand::random(),
            rtp_timestamp: rand::random(),
            loop_playback,
            completion_notify,
            completion_fired: false,
            finished: false,
        })
    }

    fn fire_completion_once(&mut self) {
        if !self.completion_fired {
            self.completion_fired = true;
            self.completion_notify.notify_waiters();
        }
    }

    async fn finish_pending(&mut self) -> MediaResult<MediaSample> {
        self.finished = true;
        self.fire_completion_once();
        std::future::pending::<MediaResult<MediaSample>>().await
    }
}

#[async_trait]
impl MediaSource for FileSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        if self.finished {
            return std::future::pending::<MediaResult<MediaSample>>().await;
        }

        loop {
            self.ticker.tick().await;

            let read = self.audio_source_manager.read_samples(&mut self.pcm_buf);

            if read == 0 {
                if self.loop_playback {
                    continue;
                }
                return self.finish_pending().await;
            }

            let encoded = self.encoder.encode(&self.pcm_buf[..read]);
            let frame = AudioFrame {
                rtp_timestamp: self.rtp_timestamp,
                clock_rate: self.codec_info.clock_rate,
                data: Bytes::from(encoded),
                sequence_number: Some(self.sequence_number),
                payload_type: Some(self.codec_info.payload_type),
                marker: false,
                raw_packet: None,
                source_addr: None,
            };

            self.rtp_timestamp = self
                .rtp_timestamp
                .wrapping_add(self.rtp_ticks_per_frame);
            self.sequence_number = self.sequence_number.wrapping_add(1);

            return Ok(MediaSample::Audio(frame));
        }
    }
}

struct AdapterRtpState {
    next_output_timestamp: u32,
    next_output_sequence: u16,
    last_input_timestamp: Option<u32>,
    last_input_sequence: Option<u16>,
    last_clock_rate: Option<u32>,
    last_step: u32,
}

impl Default for AdapterRtpState {
    fn default() -> Self {
        Self {
            next_output_timestamp: rand::random(),
            next_output_sequence: rand::random(),
            last_input_timestamp: None,
            last_input_sequence: None,
            last_clock_rate: None,
            last_step: 160,
        }
    }
}

impl AdapterRtpState {
    fn estimate_step(frame: &AudioFrame) -> u32 {
        (frame.clock_rate / 50).max(1)
    }

    fn rewrite(&mut self, frame: &mut AudioFrame) {
        let input_timestamp = frame.rtp_timestamp;
        let input_sequence = frame.sequence_number;
        let step = match (
            self.last_input_timestamp,
            self.last_input_sequence,
            self.last_clock_rate,
            input_sequence,
        ) {
            (Some(last_ts), Some(last_seq), Some(last_rate), Some(input_seq))
                if last_rate == frame.clock_rate && input_seq == last_seq.wrapping_add(1) =>
            {
                let delta = input_timestamp.wrapping_sub(last_ts);
                if delta > 0 && delta < frame.clock_rate.saturating_mul(5) {
                    delta
                } else {
                    Self::estimate_step(frame)
                }
            }
            _ => Self::estimate_step(frame),
        };

        frame.rtp_timestamp = self.next_output_timestamp;
        frame.sequence_number = Some(self.next_output_sequence);

        self.next_output_timestamp = self.next_output_timestamp.wrapping_add(step);
        self.next_output_sequence = self.next_output_sequence.wrapping_add(1);
        self.last_input_timestamp = Some(input_timestamp);
        self.last_input_sequence = input_sequence;
        self.last_clock_rate = Some(frame.clock_rate);
        self.last_step = step;
    }
}

pub struct SenderTrackAdapter {
    id: String,
    kind: TrackMediaKind,
    current_link: Mutex<DirectedLink>,
    change_tx: mpsc::Sender<DirectedLink>,
    change_rx: Mutex<mpsc::Receiver<DirectedLink>>,
    timing: StdMutex<AdapterRtpState>,
}

impl SenderTrackAdapter {
    pub fn new(id: String, kind: TrackMediaKind) -> Self {
        let (change_tx, change_rx) = mpsc::channel(1);
        Self {
            id,
            kind,
            current_link: Mutex::new(DirectedLink::idle()),
            change_tx,
            change_rx: Mutex::new(change_rx),
            timing: StdMutex::new(AdapterRtpState::default()),
        }
    }

    pub fn replace_link(&self, link: DirectedLink) {
        let _ = self.change_tx.try_send(link);
    }

    pub fn remove_link(&self) {
        let _ = self.change_tx.try_send(DirectedLink::idle());
    }
}

#[async_trait]
impl MediaStreamTrack for SenderTrackAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> TrackMediaKind {
        self.kind
    }

    fn state(&self) -> TrackState {
        TrackState::Live
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        let mut current_link = self.current_link.lock().await;
        let mut change_rx = self.change_rx.lock().await;

        loop {
            tokio::select! {
                result = current_link.recv() => {
                    let mut sample = result?;
                    if let MediaSample::Audio(frame) = &mut sample {
                        if let Ok(mut timing) = self.timing.lock() {
                            timing.rewrite(frame);
                        }
                    }
                    return Ok(sample);
                },
                updated = change_rx.recv() => {
                    *current_link = updated.unwrap_or_else(DirectedLink::idle);
                    continue;
                }
            }
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct LegOutput {
    adapter: Arc<SenderTrackAdapter>,
}

impl LegOutput {
    pub fn attach(track_id: &str, target_pc: &PeerConnection) -> Result<Self> {
        let adapter = Arc::new(SenderTrackAdapter::new(
            track_id.to_string(),
            TrackMediaKind::Audio,
        ));

        let target_transceiver = target_pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == MediaKind::Audio)
            .ok_or_else(|| anyhow!("no audio transceiver on target pc"))?;

        let existing_sender = target_transceiver
            .sender()
            .ok_or_else(|| anyhow!("no sender on target audio transceiver"))?;

        let sender = RtpSender::builder(
            adapter.clone() as Arc<dyn MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params())
        .build();

        target_transceiver.set_sender(Some(sender));

        Ok(Self { adapter })
    }

    pub async fn replace_link(&self, link: DirectedLink) {
        self.adapter.replace_link(link);
    }

    pub async fn remove_link(&self) {
        self.adapter.remove_link();
    }

    pub async fn replace_with_file_source(&self, source: FileSource) {
        let file_link = DirectedLink::new(Box::new(source));
        self.adapter.replace_link(file_link);
    }
}

pub struct BidirectionalBridge {
    caller_source_track: Arc<dyn MediaStreamTrack>,
    callee_source_track: Arc<dyn MediaStreamTrack>,
    caller_to_callee: PeerSourceConfig,
    callee_to_caller: PeerSourceConfig,
    caller_output: LegOutput,
    callee_output: LegOutput,
}

impl BidirectionalBridge {
    pub fn new(
        caller_source_track: Arc<dyn MediaStreamTrack>,
        callee_source_track: Arc<dyn MediaStreamTrack>,
        caller_to_callee: PeerSourceConfig,
        callee_to_caller: PeerSourceConfig,
        caller_output: LegOutput,
        callee_output: LegOutput,
    ) -> Self {
        Self {
            caller_source_track,
            callee_source_track,
            caller_to_callee,
            callee_to_caller,
            caller_output,
            callee_output,
        }
    }

    pub async fn bridge(&self) {
        self.caller_output
            .replace_link(
                self.callee_to_caller
                    .clone()
                    .into_link(self.callee_source_track.clone()),
            )
            .await;
        self.callee_output
            .replace_link(
                self.caller_to_callee
                    .clone()
                    .into_link(self.caller_source_track.clone()),
            )
            .await;
    }

    pub async fn unbridge(&self) {
        self.caller_output.remove_link().await;
        self.callee_output.remove_link().await;
    }
}

#[derive(Clone)]
pub struct TranscodeSpec {
    pub source_codec: CodecType,
    pub target_codec: CodecType,
    pub target_pt: u8,
}

#[derive(Clone)]
pub struct PeerSourceConfig {
    pub transcode: Option<TranscodeSpec>,
    pub audio_mapping: Option<AudioMapping>,
    pub dtmf_mapping: Option<DtmfMapping>,
    pub recorder: Option<Arc<StdMutex<Option<Recorder>>>>,
    pub leg: Option<Leg>,
}

impl Default for PeerSourceConfig {
    fn default() -> Self {
        Self {
            transcode: None,
            audio_mapping: None,
            dtmf_mapping: None,
            recorder: None,
            leg: None,
        }
    }
}

impl PeerSourceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_transcode(mut self, transcode: Option<TranscodeSpec>) -> Self {
        self.transcode = transcode;
        self
    }

    pub fn with_audio_mapping(mut self, audio_mapping: Option<AudioMapping>) -> Self {
        self.audio_mapping = audio_mapping;
        self
    }

    pub fn with_dtmf_mapping(mut self, dtmf_mapping: Option<DtmfMapping>) -> Self {
        self.dtmf_mapping = dtmf_mapping;
        self
    }

    pub fn with_recorder(mut self, recorder: Arc<StdMutex<Option<Recorder>>>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn with_leg(mut self, leg: Leg) -> Self {
        self.leg = Some(leg);
        self
    }
}

impl PeerSourceConfig {
    pub fn into_link(self, source_track: Arc<dyn MediaStreamTrack>) -> DirectedLink {
        DirectedLink::new(Box::new(PeerSource::new(
            source_track,
            self.audio_mapping,
            self.dtmf_mapping,
            self.transcode,
            self.recorder,
            self.leg,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::create_encoder;
    use bytes::Bytes;
    use rustrtc::media::frame::AudioFrame;
    use std::collections::VecDeque;

    struct FakeSource {
        samples: VecDeque<MediaSample>,
    }

    #[async_trait]
    impl MediaSource for FakeSource {
        async fn recv(&mut self) -> MediaResult<MediaSample> {
            self.samples
                .pop_front()
                .ok_or(rustrtc::media::error::MediaError::EndOfStream)
        }
    }

    fn pcmu_frame(payload_type: u8, seq: u16, timestamp: u32) -> MediaSample {
        let mut encoder = create_encoder(CodecType::PCMU);
        let payload = encoder.encode(&vec![0i16; 160]);
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: timestamp,
            clock_rate: 8000,
            data: Bytes::from(payload),
            sequence_number: Some(seq),
            payload_type: Some(payload_type),
            marker: false,
            raw_packet: None,
            source_addr: None,
        })
    }

    #[tokio::test]
    async fn test_directed_link_remaps_same_codec_payload_type() {
        let source = FakeSource {
            samples: VecDeque::from(vec![pcmu_frame(0, 10, 160)]),
        };

        let link = DirectedLink::new(
            Box::new(source),
            vec![
                Box::new(AdmissionStage::new(Some(0), None)),
                Box::new(RtpRewriteStage::new(
                    Some(AudioMapping {
                        source_pt: 0,
                        target_pt: 8,
                        source_clock_rate: 8000,
                        target_clock_rate: 8000,
                        source_codec: CodecType::PCMU,
                        target_codec: CodecType::PCMU,
                    }),
                    None,
                    false,
                )),
            ],
        );

        let mut link = link;
        let MediaSample::Audio(frame) = link.recv().await.unwrap() else {
            panic!("expected audio frame");
        };

        assert_eq!(frame.payload_type, Some(8));
    }

    #[tokio::test]
    async fn test_directed_link_drops_unmapped_dtmf_and_continues() {
        let dtmf = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: Bytes::from_static(&[5, 0x80, 0x00, 0xA0]),
            sequence_number: Some(1),
            payload_type: Some(101),
            marker: false,
            raw_packet: None,
            source_addr: None,
        });
        let source = FakeSource {
            samples: VecDeque::from(vec![dtmf, pcmu_frame(0, 2, 160)]),
        };

        let mut link = DirectedLink::new(
            Box::new(source),
            vec![
                Box::new(AdmissionStage::new(Some(0), Some(101))),
                Box::new(DtmfStage::new(Some(DtmfMapping {
                    source_pt: 101,
                    target_pt: None,
                    source_clock_rate: 8000,
                    target_clock_rate: None,
                }))),
            ],
        );

        let MediaSample::Audio(frame) = link.recv().await.unwrap() else {
            panic!("expected audio frame");
        };
        assert_eq!(frame.payload_type, Some(0));
    }

    #[tokio::test]
    async fn test_directed_link_transcodes_and_rewrites() {
        let source = FakeSource {
            samples: VecDeque::from(vec![pcmu_frame(0, 10, 160)]),
        };

        let mut link = DirectedLink::new(
            Box::new(source),
            vec![
                Box::new(AdmissionStage::new(Some(0), None)),
                Box::new(TranscodeStage::new(
                    0,
                    Transcoder::new(CodecType::PCMU, CodecType::PCMA, 8),
                )),
                Box::new(RtpRewriteStage::new(
                    Some(AudioMapping {
                        source_pt: 0,
                        target_pt: 8,
                        source_clock_rate: 8000,
                        target_clock_rate: 8000,
                        source_codec: CodecType::PCMU,
                        target_codec: CodecType::PCMA,
                    }),
                    None,
                    true,
                )),
            ],
        );

        let MediaSample::Audio(frame) = link.recv().await.unwrap() else {
            panic!("expected audio frame");
        };

        assert_eq!(frame.payload_type, Some(8));
        assert_eq!(frame.clock_rate, 8000);
    }
}
