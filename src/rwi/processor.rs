use crate::callrecord::CallRecordHangupReason;
use crate::media;
use crate::media::mixer_registry::MixerParticipantRole;
use crate::media::negotiate::{CodecInfo, MediaNegotiator};
use crate::media::{FileTrack, MediaStreamBuilder, RtpTrackBuilder, Track};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::media_bridge::MediaBridge;
use crate::proxy::proxy_call::media_peer::{MediaPeer, VoiceEnginePeer};
use crate::proxy::proxy_call::session_timer::{
    HEADER_SESSION_EXPIRES, TIMER_TAG, SessionRefresher, SessionTimerState, parse_session_expires,
};
use crate::proxy::proxy_call::state::{CallSessionHandle, SessionAction};
use crate::rwi::call_leg::LegCommand;
use crate::proxy::server::SipServerRef;
use crate::rwi::call_leg::{RwiCallLeg, RwiCallLegHandle, RwiCallLegOrigin, RwiCallLegState};
use crate::rwi::gateway::RwiGateway;
use crate::rwi::proto::RwiEvent;
use crate::rwi::session::{
    ConferenceCreateRequest, OriginateRequest, QueueEnqueueRequest, RecordStartRequest,
    RwiCommandPayload, SupervisorMode,
};
use audio_codec::CodecType;
use futures::FutureExt;
use rustrtc::{Direction, MediaKind, SdpType, SessionDescription};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct QueueState {
    queue_id: String,
    _priority: Option<u32>,
    _skills: Option<Vec<String>>,
    _max_wait_secs: Option<u32>,
    is_hold: bool,
}

#[derive(Clone)]
struct RecordState {
    recording_id: String,
    _mode: String,
    _path: String,
    is_paused: bool,
}

#[derive(Clone)]
struct RingbackState {
    _target_call_id: String,
    _source_call_id: String,
}

#[derive(Clone)]
struct SupervisorState {
    _supervisor_call_id: String,
    _target_call_id: String,
    _mode: SupervisorMode,
}

#[derive(Clone)]
struct MediaStreamState {
    #[allow(dead_code)]
    call_id: String,
    #[allow(dead_code)]
    stream_id: String,
    #[allow(dead_code)]
    direction: String,
}

#[derive(Clone)]
struct MediaInjectState {
    #[allow(dead_code)]
    call_id: String,
    #[allow(dead_code)]
    stream_id: String,
    #[allow(dead_code)]
    codec: String,
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u32,
}

#[allow(dead_code)]
#[derive(Clone)]
#[allow(unused)]
struct ConferenceState {
    conf_id: String,
    backend: String,
    max_members: Option<u32>,
    record: bool,
    mcu_uri: Option<String>,
    members: Vec<String>,
}

pub struct RwiCommandProcessor {
    call_registry: Arc<ActiveProxyCallRegistry>,
    gateway: Arc<RwLock<RwiGateway>>,
    sip_server: Option<SipServerRef>,
    queue_states: Arc<RwLock<HashMap<String, QueueState>>>,
    record_states: Arc<RwLock<HashMap<String, RecordState>>>,
    ringback_states: Arc<RwLock<HashMap<String, RingbackState>>>,
    supervisor_states: Arc<RwLock<HashMap<String, SupervisorState>>>,
    media_stream_states: Arc<RwLock<HashMap<String, MediaStreamState>>>,
    media_inject_states: Arc<RwLock<HashMap<String, MediaInjectState>>>,
    mixer_registry: Arc<media::mixer_registry::MixerRegistry>,
    conference_states: Arc<RwLock<HashMap<String, ConferenceState>>>,
}

impl RwiCommandProcessor {
    fn local_contact_uri(server: &crate::proxy::server::SipServerInner) -> Option<rsip::Uri> {
        server.default_contact_uri()
    }

    fn standalone_negotiation_pending_error(pending: Option<String>) -> CommandError {
        let suffix = pending
            .map(|method| format!(" ({})", method))
            .unwrap_or_default();
        CommandError::CommandFailed(format!(
            "standalone renegotiation already in progress{}",
            suffix
        ))
    }

    pub fn new(
        call_registry: Arc<ActiveProxyCallRegistry>,
        gateway: Arc<RwLock<RwiGateway>>,
    ) -> Self {
        Self {
            call_registry,
            gateway,
            sip_server: None,
            queue_states: Arc::new(RwLock::new(HashMap::new())),
            record_states: Arc::new(RwLock::new(HashMap::new())),
            ringback_states: Arc::new(RwLock::new(HashMap::new())),
            supervisor_states: Arc::new(RwLock::new(HashMap::new())),
            media_stream_states: Arc::new(RwLock::new(HashMap::new())),
            media_inject_states: Arc::new(RwLock::new(HashMap::new())),
            mixer_registry: Arc::new(media::mixer_registry::MixerRegistry::new()),
            conference_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_sip_server(mut self, server: SipServerRef) -> Self {
        self.sip_server = Some(server);
        self
    }

    pub async fn process_command(
        &self,
        command: RwiCommandPayload,
    ) -> Result<CommandResult, CommandError> {
        match command {
            RwiCommandPayload::ListCalls => {
                let calls = self.list_calls().await;
                Ok(CommandResult::ListCalls(calls))
            }
            RwiCommandPayload::AttachCall { call_id, mode: _ } => {
                self.get_leg(&call_id).await?;
                Ok(CommandResult::CallFound { call_id })
            }
            RwiCommandPayload::Answer { call_id } => self.answer_call(&call_id).await,
            RwiCommandPayload::Hangup {
                call_id,
                reason,
                code,
            } => self.hangup_call(&call_id, reason, code).await,
            RwiCommandPayload::Reject { call_id, reason } => {
                self.reject_call(&call_id, reason).await
            }
            RwiCommandPayload::Ring { call_id } => self.ring_call(&call_id).await,
            RwiCommandPayload::Bridge { leg_a, leg_b } => self.bridge_calls(&leg_a, &leg_b).await,
            RwiCommandPayload::Unbridge { call_id } => self.unbridge_call(&call_id).await,
            RwiCommandPayload::Transfer { call_id, target } => {
                self.transfer_call(&call_id, &target).await
            }
            RwiCommandPayload::TransferAttended {
                call_id,
                target,
                timeout_secs,
            } => {
                self.transfer_attended(&call_id, &target, timeout_secs)
                    .await
            }
            RwiCommandPayload::TransferComplete {
                call_id,
                consultation_call_id,
            } => {
                self.transfer_complete(&call_id, &consultation_call_id)
                    .await
            }
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => self.transfer_cancel(&consultation_call_id).await,
            RwiCommandPayload::CallHold { call_id, music } => {
                self.call_hold(&call_id, music.as_deref()).await
            }
            RwiCommandPayload::CallUnhold { call_id } => self.call_unhold(&call_id).await,
            RwiCommandPayload::Originate(req) => self.originate_call(req).await,
            RwiCommandPayload::MediaPlay(req) => {
                self.media_play(&req.call_id, &req.source, req.interrupt_on_dtmf)
                    .await
            }
            RwiCommandPayload::MediaStop { call_id } => self.media_stop(&call_id).await,
            RwiCommandPayload::Subscribe { .. } => Ok(CommandResult::Success),
            RwiCommandPayload::Unsubscribe { .. } => Ok(CommandResult::Success),
            RwiCommandPayload::DetachCall { call_id } => {
                let known_leg = {
                    let gw = self.gateway.read().await;
                    gw.get_leg(&call_id).is_some()
                };
                if known_leg || self.call_registry.get_handle(&call_id).is_some() {
                    Ok(CommandResult::Success)
                } else {
                    Err(CommandError::CallNotFound(call_id))
                }
            }
            RwiCommandPayload::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => {
                self.set_ringback_source(&target_call_id, &source_call_id)
                    .await
            }
            RwiCommandPayload::MediaStreamStart(req) => {
                let stream_id = Uuid::new_v4().to_string();
                self.media_stream_start(&req.call_id, &stream_id, &req.direction)
                    .await
            }
            RwiCommandPayload::MediaStreamStop { call_id } => {
                self.media_stream_stop(&call_id).await
            }
            RwiCommandPayload::MediaInjectStart(req) => {
                let stream_id = Uuid::new_v4().to_string();
                self.media_inject_start(&req.call_id, &stream_id, &req.format)
                    .await
            }
            RwiCommandPayload::MediaInjectStop { call_id } => {
                self.media_inject_stop(&call_id).await
            }
            RwiCommandPayload::RecordStart(req) => self.record_start(req).await,
            RwiCommandPayload::RecordPause { call_id } => self.record_pause(&call_id).await,
            RwiCommandPayload::RecordResume { call_id } => self.record_resume(&call_id).await,
            RwiCommandPayload::RecordStop { call_id } => self.record_stop(&call_id).await,
            RwiCommandPayload::RecordMaskSegment {
                call_id,
                recording_id,
                start_secs,
                end_secs,
            } => {
                self.record_mask_segment(&call_id, &recording_id, start_secs, end_secs)
                    .await
            }
            RwiCommandPayload::QueueEnqueue(req) => self.queue_enqueue(req).await,
            RwiCommandPayload::QueueDequeue { call_id } => self.queue_dequeue(&call_id).await,
            RwiCommandPayload::QueueHold { call_id } => self.queue_hold(&call_id).await,
            RwiCommandPayload::QueueUnhold { call_id } => self.queue_unhold(&call_id).await,
            RwiCommandPayload::QueueSetPriority { call_id, priority } => {
                self.queue_set_priority(&call_id, priority).await
            }
            RwiCommandPayload::QueueAssignAgent { call_id, agent_id } => {
                self.queue_assign_agent(&call_id, &agent_id).await
            }
            RwiCommandPayload::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => self.queue_requeue(&call_id, &queue_id, priority).await,
            RwiCommandPayload::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => {
                self.supervisor_listen(&supervisor_call_id, &target_call_id)
                    .await
            }
            RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.supervisor_whisper(&supervisor_call_id, &target_call_id, &agent_leg)
                    .await
            }
            RwiCommandPayload::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.supervisor_barge(&supervisor_call_id, &target_call_id, &agent_leg)
                    .await
            }
            RwiCommandPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => {
                self.supervisor_stop(&supervisor_call_id, &target_call_id)
                    .await
            }
            RwiCommandPayload::SupervisorTakeover {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.supervisor_takeover(&supervisor_call_id, &target_call_id, &agent_leg)
                    .await
            }
            RwiCommandPayload::SipMessage {
                call_id,
                content_type,
                body,
            } => self.sip_message(&call_id, &content_type, &body).await,
            RwiCommandPayload::SipNotify {
                call_id,
                event,
                content_type,
                body,
            } => {
                self.sip_notify(&call_id, &event, &content_type, &body)
                    .await
            }
            RwiCommandPayload::SipOptionsPing { call_id } => self.sip_options_ping(&call_id).await,
            RwiCommandPayload::ConferenceCreate(req) => self.conference_create(req).await,
            RwiCommandPayload::ConferenceAdd { conf_id, call_id } => {
                self.conference_add(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceRemove { conf_id, call_id } => {
                self.conference_remove(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceMute { conf_id, call_id } => {
                self.conference_mute(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceUnmute { conf_id, call_id } => {
                self.conference_unmute(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceDestroy { conf_id } => {
                self.conference_destroy(&conf_id).await
            }
        }
    }

    fn select_best_audio_from_sdp(
        sdp: &str,
    ) -> Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)> {
        let extracted = MediaNegotiator::extract_codec_params(sdp);
        let dtmf = extracted.dtmf.clone();
        MediaNegotiator::select_best_codec(&extracted.audio, &[])
            .map(|codec| (codec.codec, codec.to_params(), dtmf))
    }

    async fn start_file_playback_on_peer(
        peer: Arc<dyn MediaPeer>,
        file_path: &str,
        internal_track_id: &str,
        loop_playback: bool,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<()> {
        if file_path.is_empty() {
            return Ok(());
        }

        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !Path::new(file_path).exists() {
            return Err(anyhow::anyhow!("Audio file not found: {}", file_path));
        }

        peer.remove_track(internal_track_id, true).await;

        let peer_tracks = peer.get_tracks().await;
        let mut codec_info = None;
        let mut target_pc = None;
        for track in &peer_tracks {
            let guard = track.lock().await;
            if guard.id() == internal_track_id {
                continue;
            }
            if target_pc.is_none() {
                target_pc = guard.get_peer_connection().await;
            }
            codec_info = guard.preferred_codec_info();
            if codec_info.is_some() {
                break;
            }
        }

        let preferred_codec = codec_info
            .as_ref()
            .map(|info| info.codec)
            .unwrap_or(CodecType::PCMU);

        let mut track = FileTrack::new(internal_track_id.to_string())
            .with_path(file_path.to_string())
            .with_loop(loop_playback)
            .with_cancel_token(cancel_token.child_token())
            .with_codec_preference(vec![preferred_codec]);
        if let Some(info) = codec_info {
            track = track.with_codec_info(info);
        }

        track.start_playback_on(target_pc).await?;
        peer.update_track(Box::new(track), Some(internal_track_id.to_string()))
            .await;
        Ok(())
    }

    async fn bridge_id_for_legs(&self, leg_a: &str, leg_b: &str) -> String {
        if leg_a <= leg_b {
            format!("{}:{}", leg_a, leg_b)
        } else {
            format!("{}:{}", leg_b, leg_a)
        }
    }

    async fn register_attached_leg(&self, call_id: &str) -> Result<RwiCallLegHandle, CommandError> {
        {
            let gw = self.gateway.read().await;
            if let Some(existing) = gw.get_leg(&call_id.to_string()) {
                return Ok(existing);
            }
        }

        let handle = self
            .call_registry
            .get_handle(call_id)
            .ok_or_else(|| CommandError::CallNotFound(call_id.to_string()))?;
        let entry = self
            .call_registry
            .get(call_id)
            .ok_or_else(|| CommandError::CallNotFound(call_id.to_string()))?;
        let leg = RwiCallLeg::new_attached(&entry, handle);
        let mut gw = self.gateway.write().await;
        gw.register_leg(call_id.to_string(), leg.clone());
        Ok(leg)
    }

    async fn get_leg(&self, call_id: &str) -> Result<RwiCallLegHandle, CommandError> {
        {
            let gw = self.gateway.read().await;
            if let Some(leg) = gw.get_leg(&call_id.to_string()) {
                return Ok(leg);
            }
        }

        if self.call_registry.get_handle(call_id).is_some() {
            let leg = self.register_attached_leg(call_id).await?;
            if leg.origin() == RwiCallLegOrigin::InboundAttached {
                return Ok(leg);
            }
            return Ok(leg);
        }

        Err(CommandError::CallNotFound(call_id.to_string()))
    }

    async fn clear_bridge(&self, call_id: &str) {
        let bridge_id = {
            let gw = self.gateway.read().await;
            gw.bridge_id_for_leg(&call_id.to_string())
        };
        let Some(bridge_id) = bridge_id else {
            return;
        };

        let bridge_state = {
            let mut gw = self.gateway.write().await;
            gw.remove_bridge_by_id(&bridge_id)
        };

        if let Some(bridge_state) = bridge_state {
            bridge_state.bridge.stop();
            let leg_a = {
                let gw = self.gateway.read().await;
                gw.get_leg(&bridge_state.leg_a)
            };
            if let Some(leg_a) = leg_a {
                leg_a.set_state(RwiCallLegState::Answered).await;
            }
            let leg_b = {
                let gw = self.gateway.read().await;
                gw.get_leg(&bridge_state.leg_b)
            };
            if let Some(leg_b) = leg_b {
                leg_b.set_state(RwiCallLegState::Answered).await;
            }
        }
    }

    async fn create_direct_bridge_with_gateway(
        gateway: Arc<RwLock<RwiGateway>>,
        sip_server: Option<SipServerRef>,
        leg_a: &str,
        leg_b: &str,
        bridge_id: String,
        emit_event: bool,
    ) -> Result<(), CommandError> {
        let leg_a_handle = {
            let gw = gateway.read().await;
            gw.get_leg(&leg_a.to_string())
        }
        .ok_or_else(|| CommandError::CallNotFound(leg_a.to_string()))?;
        let leg_b_handle = {
            let gw = gateway.read().await;
            gw.get_leg(&leg_b.to_string())
        }
        .ok_or_else(|| CommandError::CallNotFound(leg_b.to_string()))?;
        if let Some(handle) = leg_a_handle.session_handle() {
            handle.cancel_dtmf_listener();
        }
        if let Some(handle) = leg_b_handle.session_handle() {
            handle.cancel_dtmf_listener();
        }
        let runtime_a = leg_a_handle.live_media().await.ok_or_else(|| {
            CommandError::CommandFailed(format!("call {} has no live leg state", leg_a))
        })?;
        let runtime_b = leg_b_handle.live_media().await.ok_or_else(|| {
            CommandError::CommandFailed(format!("call {} has no live leg state", leg_b))
        })?;
        let (codec_a, params_a, dtmf_a) = runtime_a.negotiated_audio.clone();
        let (codec_b, params_b, dtmf_b) = runtime_b.negotiated_audio.clone();

        let bridge = Arc::new(MediaBridge::new(
            runtime_a.peer,
            runtime_b.peer,
            params_a,
            params_b,
            dtmf_a,
            dtmf_b,
            codec_a,
            codec_b,
            runtime_a.ssrc,
            runtime_b.ssrc,
            None,
            bridge_id.clone(),
            sip_server
                .as_ref()
                .and_then(|server| server.sip_flow.as_ref().and_then(|sf| sf.backend())),
        ));
        bridge
            .start()
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        {
            let mut gw = gateway.write().await;
            gw.register_bridge(
                bridge_id,
                crate::rwi::gateway::RwiBridgeState {
                    leg_a: leg_a.to_string(),
                    leg_b: leg_b.to_string(),
                    bridge,
                },
            );
        }
        leg_a_handle.mark_bridged().await;
        leg_b_handle.mark_bridged().await;

        if emit_event {
            let event = RwiEvent::CallBridged {
                leg_a: leg_a.to_string(),
                leg_b: leg_b.to_string(),
            };
            let gw = gateway.read().await;
            gw.send_event_to_call_owner(&leg_a.to_string(), &event);
            gw.send_event_to_call_owner(&leg_b.to_string(), &event);
        }

        Ok(())
    }

    async fn rebuild_direct_bridge_for_leg(
        gateway: Arc<RwLock<RwiGateway>>,
        sip_server: Option<SipServerRef>,
        call_id: &str,
    ) -> Result<(), CommandError> {
        let removed_bridge = {
            let mut gw = gateway.write().await;
            let Some(bridge_id) = gw.bridge_id_for_leg(&call_id.to_string()) else {
                return Ok(());
            };
            gw.remove_bridge_by_id(&bridge_id).map(|state| (bridge_id, state))
        };

        let Some((bridge_id, bridge_state)) = removed_bridge else {
            return Ok(());
        };

        bridge_state.bridge.stop();
        let rebuild_result = Self::create_direct_bridge_with_gateway(
                gateway.clone(),
                sip_server,
                &bridge_state.leg_a,
                &bridge_state.leg_b,
                bridge_id,
                false,
            )
            .await;

        if rebuild_result.is_err() {
            let leg_a = {
                let gw = gateway.read().await;
                gw.get_leg(&bridge_state.leg_a)
            };
            if let Some(leg_a) = leg_a {
                leg_a.set_state(RwiCallLegState::Answered).await;
            }
            let leg_b = {
                let gw = gateway.read().await;
                gw.get_leg(&bridge_state.leg_b)
            };
            if let Some(leg_b) = leg_b {
                leg_b.set_state(RwiCallLegState::Answered).await;
            }
        }

        rebuild_result
    }

    async fn apply_standalone_remote_answer(
        gateway: Arc<RwLock<RwiGateway>>,
        sip_server: Option<SipServerRef>,
        call_id: &str,
        leg: &RwiCallLegHandle,
        peer: Arc<dyn MediaPeer>,
        track_id: &str,
        answer_sdp: &str,
    ) -> Result<(), CommandError> {
        peer.update_remote_description(track_id, answer_sdp)
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        leg.set_answer(answer_sdp.to_string()).await;
        leg.set_negotiated_media(
            Self::select_best_audio_from_sdp(answer_sdp),
            MediaNegotiator::extract_ssrc(answer_sdp),
        )
        .await;
        if leg.state().await != RwiCallLegState::Bridged {
            leg.set_state(RwiCallLegState::Answered).await;
        }
        Self::rebuild_direct_bridge_for_leg(gateway, sip_server, call_id).await?;
        Ok(())
    }

    fn build_standalone_session_timer(
        enabled: bool,
        response: &rsip::Response,
        default_expires: u64,
    ) -> Option<SessionTimerState> {
        if !enabled {
            return None;
        }

        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.last_refresh = Instant::now();

        let session_expires = response.headers.iter().find_map(|header| match header {
            rsip::Header::Other(name, value) if name.eq_ignore_ascii_case(HEADER_SESSION_EXPIRES) => {
                Some(value.clone())
            }
            _ => None,
        });

        if let Some(value) = session_expires {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                timer.session_interval = interval;
                timer.refresher = refresher.unwrap_or(SessionRefresher::Uac);
                return Some(timer);
            }
        }

        timer.session_interval = std::time::Duration::from_secs(default_expires);
        timer.refresher = SessionRefresher::Uac;
        Some(timer)
    }

    async fn handle_standalone_session_timer_tick(
        call_id: &str,
        leg: &RwiCallLegHandle,
        client_dialog: Option<&rsipstack::dialog::client_dialog::ClientInviteDialog>,
        gateway: Arc<RwLock<RwiGateway>>,
    ) -> Result<bool, CommandError> {
        let Some(timer) = leg.session_timer().await else {
            return Ok(false);
        };

        if !timer.enabled || !timer.active {
            return Ok(false);
        }

        if leg.is_negotiating().await {
            return Ok(false);
        }

        if timer.is_expired() {
            warn!(call_id = %call_id, "Standalone RWI leg session timer expired");
            leg.set_state(RwiCallLegState::Terminated).await;
            {
                let gw = gateway.read().await;
                gw.send_event_to_call_owner(
                    &call_id.to_string(),
                    &RwiEvent::CallHangup {
                        call_id: call_id.to_string(),
                        reason: Some("session_timer_expired".to_string()),
                        sip_status: None,
                    },
                );
            }
            if let Some(dialog) = client_dialog {
                let _ = dialog.hangup().await;
            }
            return Ok(true);
        }

        if timer.refresher != SessionRefresher::Uac || !timer.should_refresh() {
            return Ok(false);
        }

        let Some(dialog) = client_dialog else {
            return Ok(false);
        };

        if leg.try_begin_negotiation("UPDATE").await.is_err() {
            return Ok(false);
        }
        leg.set_session_timer_refreshing(true).await;
        let headers = vec![
            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG).into()),
            rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                format!("{};refresher=uac", timer.session_interval.as_secs()),
            ),
        ];

        match dialog.update(Some(headers), None).await {
            Err(error) => {
                warn!(call_id = %call_id, error = %error, "Standalone session refresh UPDATE failed");
                leg.update_session_timer_after_refresh(false).await;
            }
            Ok(None) => {
                warn!(call_id = %call_id, "Standalone session refresh UPDATE returned no response");
                leg.update_session_timer_after_refresh(false).await;
            }
            Ok(Some(response)) => {
                if matches!(response.status_code.kind(), rsip::status_code::StatusCodeKind::Successful)
                {
                    leg.update_session_timer_after_refresh(true).await;
                } else {
                    warn!(call_id = %call_id, status = %response.status_code, "Standalone session refresh UPDATE failed");
                    leg.update_session_timer_after_refresh(false).await;
                }
            }
        }
        leg.finish_negotiation().await;

        Ok(false)
    }

    fn rewrite_audio_direction_in_sdp(
        offer_sdp: &str,
        direction: &str,
    ) -> Result<String, CommandError> {
        let mut desc = SessionDescription::parse(SdpType::Offer, offer_sdp)
            .map_err(|e| CommandError::CommandFailed(format!("failed to parse local SDP offer: {e:?}")))?;
        let direction = match direction {
            "sendonly" => Direction::SendOnly,
            "recvonly" => Direction::RecvOnly,
            "inactive" => Direction::Inactive,
            _ => Direction::SendRecv,
        };

        let mut updated = false;
        for section in &mut desc.media_sections {
            if section.kind != MediaKind::Audio {
                continue;
            }
            section.direction = direction;
            updated = true;
        }

        if !updated {
            return Err(CommandError::CommandFailed(
                "local offer missing audio media section".to_string(),
            ));
        }

        Ok(desc.to_sdp_string())
    }

    async fn build_standalone_local_offer(
        peer: Arc<dyn MediaPeer>,
        track_id: &str,
        direction: Option<&str>,
    ) -> Result<String, CommandError> {
        let tracks = peer.get_tracks().await;
        for track in tracks {
            let guard = track.lock().await;
            if guard.id() != track_id {
                continue;
            }
            let offer = guard
                .local_description()
                .await
                .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
            return match direction {
                Some(direction) => Self::rewrite_audio_direction_in_sdp(&offer, direction),
                None => Ok(offer),
            };
        }

        Err(CommandError::CommandFailed(format!(
            "standalone track not found: {}",
            track_id
        )))
    }

    async fn send_standalone_local_reinvite(
        gateway: Arc<RwLock<RwiGateway>>,
        sip_server: Option<SipServerRef>,
        call_id: &str,
        leg: &RwiCallLegHandle,
        peer: Arc<dyn MediaPeer>,
        track_id: &str,
        dialog: &rsipstack::dialog::client_dialog::ClientInviteDialog,
        direction: Option<&str>,
    ) -> Result<(), CommandError> {
        leg.try_begin_negotiation("INVITE")
            .await
            .map_err(Self::standalone_negotiation_pending_error)?;
        let local_offer = match Self::build_standalone_local_offer(peer.clone(), track_id, direction).await {
            Ok(offer) => offer,
            Err(error) => {
                leg.finish_negotiation().await;
                return Err(error);
            }
        };
        leg.set_offer(Some(local_offer.clone())).await;

        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        let response = match dialog
            .reinvite(Some(headers), Some(local_offer.into_bytes()))
            .await
        {
            Ok(Some(response)) => response,
            Ok(None) => {
                leg.finish_negotiation().await;
                return Err(CommandError::CommandFailed(
                    "standalone re-INVITE returned no response".to_string(),
                ));
            }
            Err(error) => {
                leg.finish_negotiation().await;
                return Err(CommandError::CommandFailed(error.to_string()));
            }
        };

        leg.refresh_session_timer_from_headers(&response.headers).await;
        if !matches!(response.status_code.kind(), rsip::status_code::StatusCodeKind::Successful) {
            leg.finish_negotiation().await;
            return Err(CommandError::CommandFailed(format!(
                "standalone re-INVITE failed: {}",
                response.status_code
            )));
        }

        if response.body().is_empty() {
            leg.finish_negotiation().await;
            return Err(CommandError::CommandFailed(
                "standalone re-INVITE missing SDP answer".to_string(),
            ));
        }

        let answer_sdp = String::from_utf8_lossy(response.body()).to_string();
        let result = Self::apply_standalone_remote_answer(
            gateway,
            sip_server,
            call_id,
            leg,
            peer,
            track_id,
            &answer_sdp,
        )
        .await;
        leg.finish_negotiation().await;
        result
    }

    async fn apply_standalone_remote_offer(
        gateway: Arc<RwLock<RwiGateway>>,
        sip_server: Option<SipServerRef>,
        call_id: &str,
        leg: &RwiCallLegHandle,
        peer: Arc<dyn MediaPeer>,
        track_id: &str,
        offer_sdp: &str,
    ) -> Result<String, CommandError> {
        let local_answer = peer
            .renegotiate_track(track_id, offer_sdp)
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        leg.set_offer(Some(local_answer.clone())).await;
        leg.set_answer(offer_sdp.to_string()).await;
        leg.set_negotiated_media(
            Self::select_best_audio_from_sdp(offer_sdp),
            MediaNegotiator::extract_ssrc(offer_sdp),
        )
        .await;
        if leg.state().await != RwiCallLegState::Bridged {
            leg.set_state(RwiCallLegState::Answered).await;
        }
        Self::rebuild_direct_bridge_for_leg(gateway, sip_server, call_id).await?;
        Ok(local_answer)
    }

    pub async fn originate_call(
        &self,
        req: OriginateRequest,
    ) -> Result<CommandResult, CommandError> {
        let server = self
            .sip_server
            .as_ref()
            .ok_or_else(|| CommandError::CommandFailed("SIP server not available".into()))?
            .clone();

        // Parse destination URI
        let destination_uri: rsip::Uri =
            rsip::Uri::try_from(req.destination.as_str()).map_err(|_| {
                CommandError::CommandFailed(format!("invalid destination: {}", req.destination))
            })?;

        // Resolve caller URI — use addr as fallback realm if no realms configured
        let realm = server
            .proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| server.proxy_config.addr.clone());
        let caller_str = req
            .caller_id
            .clone()
            .unwrap_or_else(|| format!("sip:rwi@{}", realm));
        let caller_uri: rsip::Uri = rsip::Uri::try_from(caller_str.as_str())
            .map_err(|_| CommandError::CommandFailed("invalid caller_id".into()))?;

        // Build extra headers
        let mut headers: Vec<rsip::Header> = vec![rsip::headers::MaxForwards::from(70u32).into()];
        for (k, v) in &req.extra_headers {
            headers.push(rsip::Header::Other(k.clone().into(), v.clone()));
        }
        if server.proxy_config.session_timer {
            let session_expires = server.proxy_config.session_expires.unwrap_or(1800);
            headers.push(rsip::headers::Supported::from(TIMER_TAG).into());
            headers.push(rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                session_expires.to_string(),
            ));
        }

        let cancel_token = CancellationToken::new();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id(format!("{}-rwi-leg", req.call_id))
                .with_cancel_token(cancel_token.child_token())
                .build(),
        )));

        // Generate SDP offer using a persistent leg-owned track.
        let track_id = format!("rwi-originate-{}", req.call_id);
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(peer.cancel_token());

        if let Some(ref ext_ip) = server.rtp_config.external_ip {
            track_builder = track_builder.with_external_ip(ext_ip.clone());
        }
        if let (Some(start), Some(end)) = (server.rtp_config.start_port, server.rtp_config.end_port)
        {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        let track = track_builder.build();
        let (offer_sdp, content_type) = match track.local_description().await {
            Ok(sdp) if !sdp.trim().is_empty() => {
                info!(call_id = %req.call_id, "Generated SDP offer for originate");
                (Some(sdp), Some("application/sdp".to_string()))
            }
            Ok(_) => {
                warn!(call_id = %req.call_id, "Generated empty SDP for originate, sending without offer");
                (None, None)
            }
            Err(e) => {
                warn!(call_id = %req.call_id, error = %e, "Failed to generate SDP for originate, sending without offer");
                (None, None)
            }
        };
        peer.update_track(Box::new(track), None).await;

        let contact_uri = Self::local_contact_uri(&server).unwrap_or_else(|| caller_uri.clone());
        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: contact_uri,
            content_type,
            offer: offer_sdp.clone().map(|sdp| sdp.into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(req.call_id.clone()),
            ..Default::default()
        };

        let call_id = req.call_id.clone();
        let gateway = self.gateway.clone();
        let sip_server = self.sip_server.clone();
        let timeout_secs = req.timeout_secs.unwrap_or(60);
        let dialog_layer = server.dialog_layer.clone();
        let session_timer_enabled = server.proxy_config.session_timer;
        let default_session_expires = server.proxy_config.session_expires.unwrap_or(1800);
        let caller_display = req.caller_id.unwrap_or_else(|| caller_str.clone());
        let callee_display = req.destination.clone();
        let (leg_cmd_tx, mut leg_cmd_rx) = mpsc::unbounded_channel();
        let leg = RwiCallLeg::new_outbound(
            call_id.clone(),
            leg_cmd_tx,
            peer.clone(),
            offer_sdp.clone(),
            cancel_token.clone(),
            Some(caller_display.clone()),
            Some(callee_display.clone()),
        );
        {
            let mut gw = self.gateway.write().await;
            gw.register_leg(call_id.clone(), leg.clone());
        }

        tokio::spawn(async move {
            let (state_tx, mut state_rx) = mpsc::unbounded_channel();
            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();
            let mut timeout =
                tokio::time::sleep(std::time::Duration::from_secs(timeout_secs as u64)).boxed();
            let mut invite_completed = false;
            let mut client_dialog: Option<rsipstack::dialog::client_dialog::ClientInviteDialog> =
                None;
            let mut timer_tick = tokio::time::interval(std::time::Duration::from_secs(1));

            loop {
                tokio::select! {
                    _ = &mut timeout, if !invite_completed => {
                        leg.set_state(RwiCallLegState::Failed).await;
                        let gw = gateway.read().await;
                        gw.send_event_to_call_owner(
                            &call_id,
                            &RwiEvent::CallNoAnswer { call_id: call_id.clone() },
                        );
                        break;
                    }
                    cmd = leg_cmd_rx.recv() => {
                        match cmd {
                            Some(LegCommand::Hangup {
                                reason,
                                code,
                                initiator,
                            }) => {
                                let _ = (reason, code, initiator);
                                if let Some(dialog) = client_dialog.as_ref() {
                                    let _ = dialog.hangup().await;
                                }
                                break;
                            }
                            Some(LegCommand::Hold { music_source }) => {
                                if let Some(dialog) = client_dialog.as_ref() {
                                    if let Err(error) = Self::send_standalone_local_reinvite(
                                        gateway.clone(),
                                        sip_server.clone(),
                                        &call_id,
                                        &leg,
                                        peer.clone(),
                                        &track_id,
                                        dialog,
                                        Some("sendonly"),
                                    )
                                    .await
                                    {
                                        warn!(call_id = %call_id, error = %error, "Failed to send standalone hold re-INVITE");
                                        continue;
                                    }
                                }
                                peer.suppress_forwarding(&track_id).await;
                                if let Some(audio_file) = music_source.as_deref().filter(|s| !s.is_empty()) {
                                    if let Err(error) = Self::start_file_playback_on_peer(
                                        peer.clone(),
                                        audio_file,
                                        "hold_music",
                                        true,
                                        &cancel_token,
                                    )
                                    .await
                                    {
                                        warn!(
                                            call_id = %call_id,
                                            file = %audio_file,
                                            error = %error,
                                            "Failed to start standalone hold playback"
                                        );
                                    }
                                }
                            }
                            Some(LegCommand::Unhold) => {
                                if let Some(dialog) = client_dialog.as_ref() {
                                    if let Err(error) = Self::send_standalone_local_reinvite(
                                        gateway.clone(),
                                        sip_server.clone(),
                                        &call_id,
                                        &leg,
                                        peer.clone(),
                                        &track_id,
                                        dialog,
                                        Some("sendrecv"),
                                    )
                                    .await
                                    {
                                        warn!(call_id = %call_id, error = %error, "Failed to send standalone unhold re-INVITE");
                                        continue;
                                    }
                                }
                                peer.remove_track("hold_music", true).await;
                                peer.resume_forwarding(&track_id).await;
                            }
                            Some(LegCommand::PlayAudio {
                                file: audio_file,
                                track_id: _,
                                loop_playback,
                            }) => {
                                if let Err(error) = Self::start_file_playback_on_peer(
                                    peer.clone(),
                                    &audio_file,
                                    "prompt",
                                    loop_playback,
                                    &cancel_token,
                                )
                                .await
                                {
                                    warn!(
                                        call_id = %call_id,
                                        file = %audio_file,
                                        error = %error,
                                        "Failed to start standalone prompt playback"
                                    );
                                }
                            }
                            Some(LegCommand::StopPlayback) => {
                                peer.remove_track("prompt", true).await;
                            }
                            Some(LegCommand::Transfer { target }) => {
                                let pending = leg.pending_negotiation_method().await;
                                let result = if let Some(dialog) = client_dialog.as_ref() {
                                    match rsip::Uri::try_from(target.as_str()) {
                                        Ok(target_uri) => dialog
                                            .refer(target_uri, None, None)
                                            .await
                                            .map_err(|error| error.to_string()),
                                        Err(error) => Err(format!(
                                            "invalid transfer target {}: {}",
                                            target, error
                                        )),
                                    }
                                } else {
                                    Err("standalone leg has no confirmed dialog for transfer".to_string())
                                };

                                match result {
                                    Ok(Some(response)) if response.status_code.kind() == rsip::StatusCodeKind::Successful => {
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallTransferred { call_id: call_id.clone() },
                                        );
                                    }
                                    Ok(Some(response)) => {
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallTransferFailed {
                                                call_id: call_id.clone(),
                                                sip_status: Some(response.status_code.code()),
                                            },
                                        );
                                    }
                                    Ok(None) => {
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallTransferFailed {
                                                call_id: call_id.clone(),
                                                sip_status: None,
                                            },
                                        );
                                    }
                                    Err(error) => {
                                        warn!(call_id = %call_id, target = %target, error = %error, pending_method = ?pending, "Failed to transfer standalone leg");
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallTransferFailed {
                                                call_id: call_id.clone(),
                                                sip_status: None,
                                            },
                                        );
                                    }
                                }
                                leg.finish_negotiation().await;
                            }
                            Some(LegCommand::SendBye { .. } | LegCommand::SendCancel) => {
                                if let Some(dialog) = client_dialog.as_ref() {
                                    let _ = dialog.hangup().await;
                                }
                                break;
                            }
                            Some(_) => {
                                // Other leg commands not applicable to standalone originate
                            }
                            None => break,
                        }
                    }
                    state = state_rx.recv() => {
                        match state {
                            Some(rsipstack::dialog::dialog::DialogState::Calling(_)) => {
                                leg.set_state(RwiCallLegState::Ringing).await;
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallRinging { call_id: call_id.clone() },
                                );
                            }
                            Some(rsipstack::dialog::dialog::DialogState::Early(_, _)) => {
                                leg.set_state(RwiCallLegState::EarlyMedia).await;
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallEarlyMedia { call_id: call_id.clone() },
                                );
                            }
                            Some(rsipstack::dialog::dialog::DialogState::Terminated(_, _)) => {
                                leg.set_state(RwiCallLegState::Terminated).await;
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallHangup {
                                        call_id: call_id.clone(),
                                        reason: Some("terminated".to_string()),
                                        sip_status: None,
                                    },
                                );
                                break;
                            }
                            Some(rsipstack::dialog::dialog::DialogState::Updated(dialog_id, request, tx_handle)) => {
                                let connected_dialog_id = leg.connected_dialog_id().await;
                                let dialog_id_string = dialog_id.to_string();
                                if connected_dialog_id.as_deref() != Some(dialog_id_string.as_str()) {
                                    debug!(call_id = %call_id, %dialog_id, "Ignoring update for stale standalone dialog");
                                    let _ = tx_handle.reply(rsip::StatusCode::CallTransactionDoesNotExist).await;
                                    continue;
                                }

                                let request_method = request.method.to_string();
                                if let Err(pending) = leg.try_begin_negotiation(&request_method).await {
                                    debug!(call_id = %call_id, %dialog_id, pending_method = ?pending, "Rejecting overlapping standalone mid-dialog negotiation");
                                    let _ = tx_handle.reply(rsip::StatusCode::RequestPending).await;
                                    continue;
                                }

                                let has_sdp = !request.body().is_empty();
                                leg.refresh_session_timer_from_headers(&request.headers).await;
                                if has_sdp {
                                    let remote_sdp = String::from_utf8_lossy(request.body()).to_string();
                                    let result = Self::apply_standalone_remote_offer(
                                        gateway.clone(),
                                        sip_server.clone(),
                                        &call_id,
                                        &leg,
                                        peer.clone(),
                                        &track_id,
                                        &remote_sdp,
                                    )
                                    .await;

                                    match result {
                                        Ok(local_answer) => {
                                            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                                            if let Err(error) = tx_handle.respond(
                                                rsip::StatusCode::OK,
                                                Some(headers),
                                                Some(local_answer.into_bytes()),
                                            ).await {
                                                warn!(call_id = %call_id, %dialog_id, error = %error, "Failed to reply to standalone mid-dialog offer");
                                            }
                                            leg.finish_negotiation().await;
                                        }
                                        Err(error) => {
                                            warn!(call_id = %call_id, %dialog_id, error = %error, "Failed to apply standalone mid-dialog offer");
                                            let _ = tx_handle.reply(rsip::StatusCode::NotAcceptableHere).await;
                                            leg.finish_negotiation().await;
                                        }
                                    }
                                } else {
                                    if let Some(local_sdp) = leg.offer_sdp().await {
                                        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                                        if let Err(error) = tx_handle.respond(
                                            rsip::StatusCode::OK,
                                            Some(headers),
                                            Some(local_sdp.into_bytes()),
                                        ).await {
                                            warn!(call_id = %call_id, %dialog_id, error = %error, "Failed to reply to standalone mid-dialog request without SDP");
                                        }
                                        leg.finish_negotiation().await;
                                    } else {
                                        let _ = tx_handle.reply(rsip::StatusCode::OK).await;
                                        leg.finish_negotiation().await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ = timer_tick.tick() => {
                        match Self::handle_standalone_session_timer_tick(
                            &call_id,
                            &leg,
                            client_dialog.as_ref(),
                            gateway.clone(),
                        )
                        .await {
                            Ok(true) => break,
                            Ok(false) => {}
                            Err(error) => {
                                warn!(call_id = %call_id, error = %error, "Standalone session timer handling failed");
                            }
                        }
                    }
                    result = &mut invitation, if !invite_completed => {
                        invite_completed = true;
                        match result {
                            Ok((new_dialog_id, Some(resp))) if resp.status_code.kind() == rsip::StatusCodeKind::Successful => {
                                leg.set_connected_dialog_id(new_dialog_id.id().to_string()).await;
                                client_dialog = Some(new_dialog_id);
                                if let Some(timer) = Self::build_standalone_session_timer(
                                    session_timer_enabled,
                                    &resp,
                                    default_session_expires,
                                ) {
                                    leg.set_session_timer(Some(timer)).await;
                                }
                                if !resp.body().is_empty() {
                                    let answer_sdp = String::from_utf8_lossy(resp.body()).to_string();
                                    if let Err(error) = Self::apply_standalone_remote_answer(
                                        gateway.clone(),
                                        sip_server.clone(),
                                        &call_id,
                                        &leg,
                                        peer.clone(),
                                        &track_id,
                                        &answer_sdp,
                                    )
                                    .await {
                                        warn!(call_id = %call_id, error = %error, "Failed to apply remote answer to standalone RWI leg");
                                    }
                                }
                                leg.set_state(RwiCallLegState::Answered).await;
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallAnswered { call_id: call_id.clone() },
                                );
                            }
                            Ok((_dialog_id, resp_opt)) => {
                                leg.set_state(RwiCallLegState::Failed).await;
                                let sip_status = resp_opt.as_ref().map(|r| r.status_code.code());
                                let gw = gateway.read().await;
                                if sip_status == Some(486) || sip_status == Some(600) {
                                    gw.send_event_to_call_owner(
                                        &call_id,
                                        &RwiEvent::CallBusy { call_id: call_id.clone() },
                                    );
                                } else {
                                    gw.send_event_to_call_owner(
                                        &call_id,
                                        &RwiEvent::CallHangup {
                                            call_id: call_id.clone(),
                                            reason: Some("originate_failed".to_string()),
                                            sip_status,
                                        },
                                    );
                                }
                                break;
                            }
                            Err(error) => {
                                leg.set_state(RwiCallLegState::Failed).await;
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallHangup {
                                        call_id: call_id.clone(),
                                        reason: Some(error.to_string()),
                                        sip_status: None,
                                    },
                                );
                                break;
                            }
                        }
                    }
                }
            }

            let bridge_id = {
                let gw = gateway.read().await;
                gw.bridge_id_for_leg(&call_id)
            };
            if let Some(bridge_id) = bridge_id {
                if let Some(bridge_state) = gateway.write().await.remove_bridge_by_id(&bridge_id) {
                    bridge_state.bridge.stop();
                }
            }
            leg.clear_runtime().await;
            gateway.write().await.remove_leg(&call_id);
            peer.stop();
        });

        Ok(CommandResult::Originated {
            call_id: req.call_id,
        })
    }

    pub async fn list_calls(&self) -> Vec<CallInfo> {
        let mut calls: Vec<_> = self
            .call_registry
            .list_recent(100)
            .into_iter()
            .map(|entry| CallInfo {
                session_id: entry.session_id,
                caller: entry.caller,
                callee: entry.callee,
                direction: entry.direction,
                status: entry.status.to_string(),
                started_at: entry.started_at.to_rfc3339(),
                answered_at: entry.answered_at.map(|t| t.to_rfc3339()),
                state: None,
            })
            .collect();
        let existing_ids: std::collections::HashSet<_> =
            calls.iter().map(|call| call.session_id.clone()).collect();
        let extra_legs = {
            let gw = self.gateway.read().await;
            gw.list_legs()
        };
        for leg in extra_legs {
            let info = leg.info().await;
            if existing_ids.contains(&info.call_id) {
                continue;
            }
            calls.push(CallInfo {
                session_id: info.call_id,
                caller: info.caller,
                callee: info.callee,
                direction: info.direction,
                status: info.status.to_string(),
                started_at: info.started_at.to_rfc3339(),
                answered_at: info.answered_at.map(|t| t.to_rfc3339()),
                state: None,
            });
        }
        calls
    }

    async fn get_handle(&self, call_id: &str) -> Result<CallSessionHandle, CommandError> {
        let leg = self.get_leg(call_id).await?;
        if !leg.supports_session_features() {
            return Err(CommandError::CommandFailed(
                "unsupported for standalone originated RWI leg".to_string(),
            ));
        }
        leg.session_handle().ok_or_else(|| {
            CommandError::CommandFailed(
                "unsupported for standalone originated RWI leg".to_string(),
            )
        })
    }

    async fn send_leg_action(
        &self,
        call_id: &str,
        action: SessionAction,
    ) -> Result<(), CommandError> {
        let leg = self.get_leg(call_id).await?;
        leg.command_handle()
            .send_action(action)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))
    }

    async fn ensure_standalone_leg_not_negotiating(
        &self,
        call_id: &str,
    ) -> Result<(), CommandError> {
        let leg = self.get_leg(call_id).await?;
        if leg.origin() == RwiCallLegOrigin::OutboundOriginated && leg.is_negotiating().await {
            return Err(Self::standalone_negotiation_pending_error(
                leg.pending_negotiation_method().await,
            ));
        }
        Ok(())
    }

    async fn answer_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::AcceptCall {
                callee: None,
                sdp: None,
                dialog_id: None,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn hangup_call(
        &self,
        call_id: &str,
        reason: Option<String>,
        code: Option<u16>,
    ) -> Result<CommandResult, CommandError> {
        let hangup_reason = reason.and_then(|r| CallRecordHangupReason::from_str(&r).ok());
        self.send_leg_action(
            call_id,
            SessionAction::Hangup {
                reason: hangup_reason,
                code,
                initiator: Some("rwi".to_string()),
            },
        )
        .await?;
        Ok(CommandResult::Success)
    }

    async fn reject_call(
        &self,
        call_id: &str,
        reason: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        let code = reason
            .as_ref()
            .and_then(|r| {
                if r.contains("busy") {
                    Some(486)
                } else if r.contains("forbidden") || r.contains("reject") {
                    Some(403)
                } else if r.contains("notfound") || r.contains("unavailable") {
                    Some(404)
                } else {
                    Some(488)
                }
            })
            .map(|c| c as u16);
        self.send_leg_action(
            call_id,
            SessionAction::Hangup {
                reason: None,
                code,
                initiator: Some("rwi".to_string()),
            },
        )
        .await?;
        Ok(CommandResult::Success)
    }

    async fn ring_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::StartRinging {
                ringback: None,
                passthrough: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn bridge_calls(&self, leg_a: &str, leg_b: &str) -> Result<CommandResult, CommandError> {
        self.clear_bridge(leg_a).await;
        self.clear_bridge(leg_b).await;
        let bridge_id = self.bridge_id_for_legs(leg_a, leg_b).await;
        Self::create_direct_bridge_with_gateway(
            self.gateway.clone(),
            self.sip_server.clone(),
            leg_a,
            leg_b,
            bridge_id,
            true,
        )
        .await?;
        Ok(CommandResult::Success)
    }

    async fn unbridge_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_leg(call_id).await?;

        let bridged = {
            let gw = self.gateway.read().await;
            gw.bridge_id_for_leg(&call_id.to_string()).is_some()
        };
        if bridged {
            self.clear_bridge(call_id).await;
        }

        let event = RwiEvent::CallUnbridged {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        Ok(CommandResult::Success)
    }

    async fn transfer_call(
        &self,
        call_id: &str,
        target: &str,
    ) -> Result<CommandResult, CommandError> {
        let leg = self.get_leg(call_id).await?;
        if leg.supports_session_features() {
            let handle = self.get_handle(call_id).await?;
            handle
                .send_command(SessionAction::from_transfer_target(target))
                .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
            return Ok(CommandResult::Success);
        }

        if leg.origin() == RwiCallLegOrigin::OutboundOriginated {
            if let Err(pending) = leg.try_begin_negotiation("REFER").await {
                return Err(Self::standalone_negotiation_pending_error(pending));
            }
            let bridged = {
                let gw = self.gateway.read().await;
                gw.bridge_id_for_leg(&call_id.to_string()).is_some()
            };
            if bridged {
                self.clear_bridge(call_id).await;
            }
            if let Err(error) = leg.command_handle().send_transfer(target.to_string()) {
                leg.finish_negotiation().await;
                return Err(CommandError::CommandFailed(error.to_string()));
            }
            return Ok(CommandResult::Success);
        }

        Ok(CommandResult::Success)
    }

    /// Initiate an attended transfer:
    /// 1. Put original call on hold
    /// 2. Originate a new call to the transfer target
    /// 3. Return the consultation call_id so client can monitor and complete/cancel
    async fn transfer_attended(
        &self,
        call_id: &str,
        target: &str,
        timeout_secs: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        // Step 1: Put original call on hold
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::Hold { music_source: None })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        // Step 2: Create consultation call to target
        let consultation_call_id = format!("consult-{}-{}", call_id, uuid::Uuid::new_v4());
        let originate_req = crate::rwi::session::OriginateRequest {
            call_id: consultation_call_id.clone(),
            destination: target.to_string(),
            caller_id: None,
            timeout_secs,
            hold_music: None,
            hold_music_target: None,
            ringback: None,
            ringback_target: None,
            extra_headers: std::collections::HashMap::new(),
        };

        self.originate_call(originate_req).await?;

        Ok(CommandResult::TransferAttended {
            original_call_id: call_id.to_string(),
            consultation_call_id,
        })
    }

    /// Complete an attended transfer:
    /// 1. Bridge original call to consultation call
    /// 2. Hang up the consultation call (which becomes the new leg)
    async fn transfer_complete(
        &self,
        call_id: &str,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Bridge the two calls together
        self.bridge_calls(call_id, consultation_call_id).await?;

        // Hang up the consultation call - this leaves caller bridged to target
        if self
            .send_leg_action(
                consultation_call_id,
                SessionAction::Hangup {
                reason: None,
                code: None,
                initiator: Some("transfer".to_string()),
                },
            )
            .await
            .is_ok()
        {}

        Ok(CommandResult::Success)
    }

    /// Cancel an attended transfer:
    /// 1. Hang up the consultation call
    /// 2. Take original call off hold
    async fn transfer_cancel(
        &self,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Hang up consultation call
        if self
            .send_leg_action(
                consultation_call_id,
                SessionAction::Hangup {
                reason: None,
                code: None,
                initiator: Some("transfer_cancel".to_string()),
                },
            )
            .await
            .is_ok()
        {}

        // Note: The original call should be automatically unheld when the consultation
        // call is hung up, as it was on hold. This is handled by the session logic.

        Ok(CommandResult::Success)
    }

    async fn media_play(
        &self,
        call_id: &str,
        source: &crate::rwi::session::MediaSource,
        interrupt_on_dtmf: bool,
    ) -> Result<CommandResult, CommandError> {
        // Resolve the audio file (or special source type) from the MediaSource.
        let source_type = source.source_type.as_str();
        let audio_file = match source_type {
            "silence" => {
                // Silence is represented by an empty file path — the app layer
                // will interpret an empty string as "produce silence".
                String::new()
            }
            "ringback" => {
                // Use the configured ringback tone; fall back to a sensible default.
                source
                    .uri
                    .clone()
                    .unwrap_or_else(|| "sounds/ringback.wav".to_string())
            }
            _ => {
                // "file" and any other type — use the provided URI.
                source.uri.clone().unwrap_or_default()
            }
        };

        let loop_playback = source.looped.unwrap_or(false);

        // Generate a unique track_id for this playback session so the caller
        // can correlate MediaPlayStarted / MediaPlayFinished events.
        let track_id = uuid::Uuid::new_v4().to_string();
        let leg = self.get_leg(call_id).await?;
        if leg.supports_session_features() {
            let handle = self.get_handle(call_id).await?;

            // Try to inject the request via the app-event channel (fast path —
            // works while an RwiApp / CallApp is running on this call).
            let delivered = handle.send_app_event(crate::call::app::ControllerEvent::Custom(
                "media.play".to_string(),
                serde_json::json!({
                    "audio_file":       audio_file,
                    "track_id":         track_id,
                    "interrupt_on_dtmf": interrupt_on_dtmf,
                    "loop":             loop_playback,
                    "source_type":      source_type,
                }),
            ));

            if !delivered {
                // Fall back to the SessionAction path (used when no CallApp is
                // currently running — e.g. the call is in the post-dialplan wait
                // loop, or for originate calls where PlayPrompt would hit the
                // action_inbox after execute_dialplan returns).
                handle
                    .send_command(SessionAction::PlayPrompt {
                        audio_file: audio_file.to_string(),
                        send_progress: false,
                        await_completion: false,
                        track_id: Some(track_id.clone()),
                        loop_playback,
                        interrupt_on_dtmf,
                    })
                    .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
            }
        } else {
            self.send_leg_action(
                call_id,
                SessionAction::PlayPrompt {
                    audio_file: audio_file.to_string(),
                    send_progress: false,
                    await_completion: false,
                    track_id: Some(track_id.clone()),
                    loop_playback,
                    interrupt_on_dtmf,
                },
            )
            .await?;
        }

        // Emit MediaPlayStarted immediately so the RWI client can correlate.
        let event = RwiEvent::MediaPlayStarted {
            call_id: call_id.to_string(),
            track_id: track_id.clone(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        Ok(CommandResult::MediaPlay { track_id })
    }

    async fn media_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.send_leg_action(call_id, SessionAction::StopPlayback)
            .await?;
        Ok(CommandResult::Success)
    }

    async fn queue_enqueue(&self, req: QueueEnqueueRequest) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(&req.call_id).await?;
        handle.set_queue_name(Some(req.queue_id.clone()));
        let queue_state = QueueState {
            queue_id: req.queue_id.clone(),
            _priority: req.priority,
            _skills: req.skills,
            _max_wait_secs: req.max_wait_secs,
            is_hold: false,
        };
        let mut states = self.queue_states.write().await;
        states.insert(req.call_id.clone(), queue_state);
        let event = RwiEvent::QueueJoined {
            call_id: req.call_id.clone(),
            queue_id: req.queue_id,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&req.call_id, &event);
        Ok(CommandResult::Success)
    }

    async fn queue_dequeue(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let queue_id = {
            let states = self.queue_states.read().await;
            states.get(call_id).map(|s| s.queue_id.clone())
        };
        handle.set_queue_name(None);
        let mut states = self.queue_states.write().await;
        states.remove(call_id);
        if let Some(qid) = queue_id {
            let event = RwiEvent::QueueLeft {
                call_id: call_id.to_string(),
                queue_id: qid,
                reason: None,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn queue_hold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_hold = true;
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }
        handle
            .send_command(SessionAction::PlayPrompt {
                audio_file: String::new(),
                send_progress: false,
                await_completion: true,
                track_id: None,
                loop_playback: true,
                interrupt_on_dtmf: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn queue_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_hold = false;
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }
        handle
            .send_command(SessionAction::StopPlayback)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    /// Set priority for a call in queue
    async fn queue_set_priority(
        &self,
        call_id: &str,
        priority: u32,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        {
            let states = self.queue_states.read().await;
            if !states.contains_key(call_id) {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }

        // Update priority in state
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state._priority = Some(priority);
            }
        }

        info!(call_id = %call_id, priority = %priority, "Queue priority updated");
        Ok(CommandResult::Success)
    }

    /// Assign agent to a call in queue
    async fn queue_assign_agent(
        &self,
        call_id: &str,
        agent_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        let queue_id = {
            let states = self.queue_states.read().await;
            if let Some(state) = states.get(call_id) {
                state.queue_id.clone()
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        // Emit agent assigned event
        let event = RwiEvent::QueueAgentOffered {
            call_id: call_id.to_string(),
            queue_id: queue_id.clone(),
            agent_id: agent_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(call_id = %call_id, agent_id = %agent_id, "Agent assigned to queue call");
        Ok(CommandResult::Success)
    }

    /// Requeue a call to a different queue
    async fn queue_requeue(
        &self,
        call_id: &str,
        queue_id: &str,
        priority: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        let old_queue_id = {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                let old = state.queue_id.clone();
                state.queue_id = queue_id.to_string();
                if let Some(p) = priority {
                    state._priority = Some(p);
                }
                old
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        // Emit requeue event
        let event = RwiEvent::QueueLeft {
            call_id: call_id.to_string(),
            queue_id: old_queue_id,
            reason: Some("requeued".to_string()),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        let event2 = RwiEvent::QueueJoined {
            call_id: call_id.to_string(),
            queue_id: queue_id.to_string(),
        };
        gw.broadcast_event(&event2);

        info!(call_id = %call_id, new_queue = %queue_id, "Call requeued");
        Ok(CommandResult::Success)
    }

    /// Place a call on hold with optional music.
    async fn call_hold(
        &self,
        call_id: &str,
        music: Option<&str>,
    ) -> Result<CommandResult, CommandError> {
        self.ensure_standalone_leg_not_negotiating(call_id).await?;
        let audio_file = music.unwrap_or("").to_string();
        self.send_leg_action(
            call_id,
            SessionAction::Hold {
                music_source: Some(audio_file),
            },
        )
        .await?;
        let event = RwiEvent::MediaHoldStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    /// Release a call from hold.
    async fn call_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.ensure_standalone_leg_not_negotiating(call_id).await?;
        self.send_leg_action(call_id, SessionAction::Unhold).await?;
        let event = RwiEvent::MediaHoldStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn record_start(&self, req: RecordStartRequest) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(&req.call_id).await?;
        let recording_id = Uuid::new_v4().to_string();
        let path = req.storage.path.clone();
        handle
            .send_command(SessionAction::StartRecording {
                path: path.clone(),
                max_duration: req
                    .max_duration_secs
                    .map(|v| std::time::Duration::from_secs(v as u64)),
                beep: req.beep.unwrap_or(false),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let record_state = RecordState {
            recording_id: recording_id.clone(),
            _mode: req.mode,
            _path: path,
            is_paused: false,
        };
        let mut states = self.record_states.write().await;
        states.insert(req.call_id.clone(), record_state);
        let event = RwiEvent::RecordStarted {
            call_id: req.call_id.clone(),
            recording_id,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&req.call_id, &event);
        Ok(CommandResult::Success)
    }

    async fn record_pause(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_paused = true;
            } else {
                return Err(CommandError::CommandFailed(
                    "No recording in progress".to_string(),
                ));
            }
        }
        handle
            .send_command(SessionAction::PauseRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let recording_id = {
            let states = self.record_states.read().await;
            states.get(call_id).map(|s| s.recording_id.clone())
        };
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordPaused {
                call_id: call_id.to_string(),
                recording_id: rid,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn record_resume(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_paused = false;
            } else {
                return Err(CommandError::CommandFailed(
                    "No recording in progress".to_string(),
                ));
            }
        }
        handle
            .send_command(SessionAction::ResumeRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let recording_id = {
            let states = self.record_states.read().await;
            states.get(call_id).map(|s| s.recording_id.clone())
        };
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordResumed {
                call_id: call_id.to_string(),
                recording_id: rid,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn record_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let (recording_id, duration) = {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.remove(call_id) {
                (Some(state.recording_id), None)
            } else {
                (None, None)
            }
        };
        handle
            .send_command(SessionAction::StopRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordStopped {
                call_id: call_id.to_string(),
                recording_id: rid,
                duration_secs: duration,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    /// Mask a segment of a recording (for PCI compliance)
    async fn record_mask_segment(
        &self,
        call_id: &str,
        recording_id: &str,
        start_secs: u64,
        end_secs: u64,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if recording exists
        {
            let states = self.record_states.read().await;
            if !states.contains_key(call_id) {
                return Err(CommandError::CommandFailed(
                    "No active recording for this call".to_string(),
                ));
            }
        }

        // Emit segment masked event (in a real implementation, this would trigger the actual masking)
        let event = RwiEvent::RecordSegmentMasked {
            call_id: call_id.to_string(),
            recording_id: recording_id.to_string(),
            start_secs,
            end_secs,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        info!(call_id = %call_id, recording_id = %recording_id, start_secs = %start_secs, end_secs = %end_secs, "Recording segment masked");
        Ok(CommandResult::Success)
    }

    async fn sip_message(
        &self,
        call_id: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let event = RwiEvent::SipMessageReceived {
            call_id: call_id.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn sip_notify(
        &self,
        call_id: &str,
        event: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let event = RwiEvent::SipNotifyReceived {
            call_id: call_id.to_string(),
            event: event.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn sip_options_ping(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        // OPTIONS ping is a keepalive probe sent to the remote party in-dialog.
        // When a real SIP stack is wired, this would send a SIP OPTIONS request
        // and resolve on 200 OK. The result (success/failure) is reported via
        // the command response; no additional event is emitted since the ping
        // is fire-and-forget from the client's perspective.
        //
        // Verify the call exists before attempting.
        self.get_handle(call_id).await?;
        Ok(CommandResult::Success)
    }

    async fn conference_create(
        &self,
        req: ConferenceCreateRequest,
    ) -> Result<CommandResult, CommandError> {
        let conf_id = req.conf_id.clone();

        // Check if conference already exists
        {
            let states = self.conference_states.read().await;
            if states.contains_key(&conf_id) {
                return Err(CommandError::CommandFailed(format!(
                    "conference {} already exists",
                    conf_id
                )));
            }
        }

        // For external backend, validate MCU URI if provided
        if req.backend == "external" {
            if req.mcu_uri.is_none() {
                return Err(CommandError::CommandFailed(
                    "external backend requires mcu_uri".to_string(),
                ));
            }
        }

        // Create conference state
        let state = ConferenceState {
            conf_id: conf_id.clone(),
            backend: req.backend,
            max_members: req.max_members,
            record: req.record,
            mcu_uri: req.mcu_uri,
            members: vec![],
        };

        // Create mixer in registry
        self.mixer_registry
            .create_conference_mixer(conf_id.clone(), 8000);

        // Store conference state
        {
            let mut states = self.conference_states.write().await;
            states.insert(conf_id.clone(), state);
        }

        // Emit event
        let event = RwiEvent::ConferenceCreated {
            conf_id: conf_id.clone(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, "Conference created");
        Ok(CommandResult::ConferenceCreated { conf_id })
    }

    async fn conference_add(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify the call exists
        self.get_handle(call_id).await?;

        // Get conference state
        let mut state = {
            let mut states = self.conference_states.write().await;
            let state = states.get_mut(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            // Check max members
            if let Some(max) = state.max_members {
                if state.members.len() >= max as usize {
                    return Err(CommandError::CommandFailed(format!(
                        "conference {} is full",
                        conf_id
                    )));
                }
            }

            state.clone()
        };

        // Add call to mixer
        let added = self.mixer_registry.add_participant(
            conf_id,
            call_id.to_string(),
            MixerParticipantRole::ConferenceParticipant,
        );

        if !added {
            return Err(CommandError::CommandFailed(format!(
                "failed to add participant to mixer"
            )));
        }

        // Update state
        state.members.push(call_id.to_string());
        {
            let mut states = self.conference_states.write().await;
            if let Some(s) = states.get_mut(conf_id) {
                s.members.push(call_id.to_string());
            }
        }

        // Emit event
        let event = RwiEvent::ConferenceMemberJoined {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member added");
        Ok(CommandResult::ConferenceMemberAdded {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_remove(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Get conference state
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!(
                    "call {} is not in conference {}",
                    call_id, conf_id
                )));
            }
        }

        // Remove from mixer
        self.mixer_registry.remove_participant(call_id);

        // Update state
        {
            let mut states = self.conference_states.write().await;
            if let Some(s) = states.get_mut(conf_id) {
                s.members.retain(|c| c != call_id);
            }
        }

        // Emit event
        let event = RwiEvent::ConferenceMemberLeft {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member removed");
        Ok(CommandResult::ConferenceMemberRemoved {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_mute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify call is in conference
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!(
                    "call {} is not in conference {}",
                    call_id, conf_id
                )));
            }
        }

        // TODO: Actually mute the participant in the mixer
        // For now, just track the muted state and emit event

        // Emit event
        let event = RwiEvent::ConferenceMemberMuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member muted");
        Ok(CommandResult::ConferenceMemberMuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_unmute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify call is in conference
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!(
                    "call {} is not in conference {}",
                    call_id, conf_id
                )));
            }
        }

        // TODO: Actually unmute the participant in the mixer
        // For now, just emit event

        // Emit event
        let event = RwiEvent::ConferenceMemberUnmuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member unmuted");
        Ok(CommandResult::ConferenceMemberUnmuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_destroy(&self, conf_id: &str) -> Result<CommandResult, CommandError> {
        // Get conference state to get members
        let members = {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;
            state.members.clone()
        };

        // Remove all participants from mixer
        for call_id in &members {
            self.mixer_registry.remove_participant(call_id);
        }

        // Remove mixer
        self.mixer_registry.remove_mixer(conf_id);

        // Remove conference state
        {
            let mut states = self.conference_states.write().await;
            states.remove(conf_id);
        }

        // Emit event
        let event = RwiEvent::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, "Conference destroyed");
        Ok(CommandResult::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        })
    }

    async fn set_ringback_source(
        &self,
        target_call_id: &str,
        source_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(target_call_id).await?;
        self.get_handle(source_call_id).await?;
        let ringback_state = RingbackState {
            _target_call_id: target_call_id.to_string(),
            _source_call_id: source_call_id.to_string(),
        };
        let mut states = self.ringback_states.write().await;
        states.insert(target_call_id.to_string(), ringback_state);
        let event = RwiEvent::MediaRingbackPassthroughStarted {
            source: source_call_id.to_string(),
            target: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&source_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_listen(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_listen: found supervisor session in registry");
                h
            }
            None => {
                // Need to create a new supervisor session and bridge to target
                // For now, we return an error indicating supervisor session must be created first
                // In a full implementation, we would auto-create and bridge here
                tracing::warn!(
                    "supervisor_listen: supervisor session {} does not exist in registry",
                    supervisor_call_id
                );
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!(
            "supervisor_listen: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_listen: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Listen,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_listen: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        // This will fail for outbound calls that don't have a command processor
        tracing::info!("supervisor_listen: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Listen,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!(
                "supervisor_listen: send_command failed (expected for outbound calls): {}",
                e
            );
        } else {
            tracing::info!("supervisor_listen: command sent successfully");
        }

        // Track the supervisor state
        tracing::info!("supervisor_listen: supervisor state tracked");

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "listen",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor listen started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Listen,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorListenStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_whisper(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
        _agent_leg: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_whisper: using existing supervisor session");
                h
            }
            None => {
                tracing::warn!(
                    "supervisor_whisper: supervisor session does not exist, must be originated first"
                );
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!(
            "supervisor_whisper: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_whisper: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Whisper,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_whisper: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        tracing::info!("supervisor_whisper: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Whisper,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!(
                "supervisor_whisper: send_command failed (expected for outbound calls): {}",
                e
            );
        }

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "whisper",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor whisper started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Whisper,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorWhisperStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_barge(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
        _agent_leg: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_barge: using existing supervisor session");
                h
            }
            None => {
                tracing::warn!(
                    "supervisor_barge: supervisor session does not exist, must be originated first"
                );
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!(
            "supervisor_barge: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_barge: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Barge,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_barge: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        tracing::info!("supervisor_barge: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Barge,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!(
                "supervisor_barge: send_command failed (expected for outbound calls): {}",
                e
            );
        } else {
            tracing::info!("supervisor_barge: command sent successfully");
        }

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "barge",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor barge started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Barge,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorBargeStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_stop(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Stop and remove the supervisor mixer
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_stop: removing mixer with id={}", mixer_id);

        // Remove the mixer from registry (this also stops it)
        let removed = self.mixer_registry.remove_mixer(&mixer_id);
        if removed {
            tracing::info!("supervisor_stop: mixer stopped and removed");
        } else {
            tracing::warn!("supervisor_stop: mixer not found (may have already been removed)");
        }

        // Send SupervisorStop action to TARGET call (the one that created the mixer)
        if let Ok(handle) = self.get_handle(target_call_id).await {
            let _ = handle.send_command(SessionAction::SupervisorStop);
        }

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "stop",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor mode stopped"
        );

        let mut states = self.supervisor_states.write().await;
        states.remove(supervisor_call_id);
        let event = RwiEvent::SupervisorModeStopped {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        if self.get_handle(target_call_id).await.is_ok() {
            gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    /// Supervisor takeover - replaces the agent on the call
    async fn supervisor_takeover(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
        agent_leg: &str,
    ) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => h,
            None => {
                return Err(CommandError::CommandFailed(format!(
                    "supervisor call {} not found",
                    supervisor_call_id
                )));
            }
        };

        // Create a mixer for the takeover
        let mixer_id = format!("takeover-{}-{}", supervisor_call_id, target_call_id);
        let _mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id.clone(),
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            crate::media::mixer::SupervisorMixerMode::Barge,
        );

        // Track supervisor state
        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Barge,
        };
        {
            let mut states = self.supervisor_states.write().await;
            states.insert(supervisor_call_id.to_string(), state);
        }

        // Send SupervisorBarge action to both calls
        supervisor_handle
            .send_command(SessionAction::SupervisorBarge {
                target_session_id: target_call_id.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let target_handle = self.get_handle(target_call_id).await?;
        target_handle
            .send_command(SessionAction::SupervisorBarge {
                target_session_id: supervisor_call_id.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        // Audit log
        info!(
            audit_event = "supervisor_action",
            action = "takeover",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            agent_leg = %agent_leg,
            result = "success",
            "Supervisor takeover completed"
        );

        // Emit takeover completed event
        let event = RwiEvent::SupervisorTakeoverCompleted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
            previous_agent_call_id: agent_leg.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);

        Ok(CommandResult::Success)
    }

    async fn media_stream_start(
        &self,
        call_id: &str,
        stream_id: &str,
        direction: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let state = MediaStreamState {
            call_id: call_id.to_string(),
            stream_id: stream_id.to_string(),
            direction: direction.to_string(),
        };
        let mut states = self.media_stream_states.write().await;
        states.insert(call_id.to_string(), state);
        let event = RwiEvent::MediaStreamStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_stream_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_stream_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_start(
        &self,
        call_id: &str,
        stream_id: &str,
        format: &crate::rwi::session::MediaFormat,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let state = MediaInjectState {
            call_id: call_id.to_string(),
            stream_id: stream_id.to_string(),
            codec: format.codec.clone(),
            sample_rate: format.sample_rate,
            channels: format.channels,
        };
        let mut states = self.media_inject_states.write().await;
        states.insert(call_id.to_string(), state);
        let event = RwiEvent::MediaStreamStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_inject_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }
}

#[derive(Debug)]
pub enum CommandResult {
    Success,
    ListCalls(Vec<CallInfo>),
    CallFound {
        call_id: String,
    },
    Originated {
        call_id: String,
    },
    MediaPlay {
        track_id: String,
    },
    TransferAttended {
        original_call_id: String,
        consultation_call_id: String,
    },
    ConferenceCreated {
        conf_id: String,
    },
    ConferenceMemberAdded {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberRemoved {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberMuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberUnmuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceDestroyed {
        conf_id: String,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallInfo {
    pub session_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub status: String,
    pub started_at: String,
    pub answered_at: Option<String>,
    pub state: Option<CallStateInfo>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallStateInfo {
    pub phase: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub hangup_reason: Option<String>,
}

#[derive(Debug)]
pub enum CommandError {
    CallNotFound(String),
    CommandFailed(String),
    NotImplemented(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::CallNotFound(id) => write!(f, "Call not found: {}", id),
            CommandError::CommandFailed(msg) => write!(f, "Command failed: {}", msg),
            CommandError::NotImplemented(feature) => write!(f, "Not implemented: {}", feature),
        }
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::media::MediaStreamBuilder;
    use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
    use crate::proxy::proxy_call::media_peer::VoiceEnginePeer;
    use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};
    use crate::rwi::gateway::RwiGateway;
    use crate::rwi::session::RwiCommandPayload;
    use audio_codec::CodecType;
    use std::sync::Arc;
    use tokio::sync::{RwLock, mpsc};
    use tokio_util::sync::CancellationToken;

    fn create_test_processor() -> Arc<RwiCommandProcessor> {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        Arc::new(RwiCommandProcessor::new(registry, gateway))
    }

    fn create_test_processor_with_registry(
        registry: Arc<ActiveProxyCallRegistry>,
    ) -> Arc<RwiCommandProcessor> {
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        Arc::new(RwiCommandProcessor::new(registry, gateway))
    }

    fn create_test_call(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> CallSessionHandle {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            direction,
            Some(caller.to_string()),
            Some(callee.to_string()),
            Some(registry.clone()),
        );
        let (handle, _rx) = CallSessionHandle::with_shared(shared);

        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        handle
    }

    /// Same as `create_test_call` but returns the `SessionActionReceiver` so tests
    /// can verify which commands are sent to the handle.
    fn create_test_call_with_rx(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> (
        CallSessionHandle,
        crate::proxy::proxy_call::state::SessionActionReceiver,
    ) {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            direction,
            Some(caller.to_string()),
            Some(callee.to_string()),
            Some(registry.clone()),
        );
        let (handle, rx) = CallSessionHandle::with_shared(shared);

        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        (handle, rx)
    }

    fn create_test_media_ready_call(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> CallSessionHandle {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            direction,
            Some(caller.to_string()),
            Some(callee.to_string()),
            Some(registry.clone()),
        );
        let (handle, _rx) = CallSessionHandle::with_shared(shared);

        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: Some(chrono::Utc::now()),
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking,
        };

        registry.upsert(entry, handle.clone());
        handle
    }

    fn publish_test_media_to_handle(handle: &CallSessionHandle, call_id: &str) {
        let peer: Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id(format!("{}-rwi-test-leg", call_id))
                .build(),
        )));
        use crate::proxy::proxy_call::state::CallSessionShared;
        handle.publish_caller_media(
            peer,
            None,
            None,
            Some((
                CodecType::PCMU,
                rustrtc::RtpCodecParameters {
                    payload_type: CodecType::PCMU.payload_type(),
                    clock_rate: CodecType::PCMU.clock_rate(),
                    channels: CodecType::PCMU.channels() as u8,
                },
                Vec::new(),
            )),
            Some(1234),
            false,
        );
    }

    async fn register_test_media_ready_rwi_leg(
        processor: &Arc<RwiCommandProcessor>,
        call_id: &str,
        handle: CallSessionHandle,
    ) {
        publish_test_media_to_handle(&handle, call_id);
        let leg = RwiCallLeg::new_attached(
            &crate::proxy::active_call_registry::ActiveProxyCallEntry {
                session_id: call_id.to_string(),
                caller: Some("1001".to_string()),
                callee: Some("2001".to_string()),
                direction: "outbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: Some(chrono::Utc::now()),
                status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking,
            },
            handle,
        );
        processor
            .gateway
            .write()
            .await
            .register_leg(call_id.to_string(), leg);
    }

    async fn register_test_standalone_media_ready_rwi_leg(
        processor: &Arc<RwiCommandProcessor>,
        call_id: &str,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {}
        });
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id(format!("{}-standalone-rwi-test-leg", call_id))
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            call_id.to_string(),
            tx,
            peer,
            Some("v=0".to_string()),
            CancellationToken::new(),
            Some("1001".to_string()),
            Some("2001".to_string()),
        );
        leg.set_negotiated_media(
            Some((
                CodecType::PCMU,
                rustrtc::RtpCodecParameters {
                    payload_type: CodecType::PCMU.payload_type(),
                    clock_rate: CodecType::PCMU.clock_rate(),
                    channels: CodecType::PCMU.channels() as u8,
                },
                Vec::new(),
            )),
            Some(1234),
        )
        .await;
        leg.set_state(crate::rwi::call_leg::RwiCallLegState::Answered)
            .await;
        processor
            .gateway
            .write()
            .await
            .register_leg(call_id.to_string(), leg);
    }

    async fn register_test_standalone_rwi_leg_with_real_track(
        processor: &Arc<RwiCommandProcessor>,
        call_id: &str,
        local_codecs: Vec<CodecType>,
        remote_codecs: Vec<CodecType>,
    ) -> (RwiCallLegHandle, Arc<dyn MediaPeer>, String) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {}
        });

        let peer: Arc<dyn MediaPeer> = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id(format!("{}-standalone-real-stream", call_id))
                .build(),
        )));
        let track_id = format!("{}-real-track", call_id);
        let local_track = RtpTrackBuilder::new(track_id.clone())
            .with_codec_preference(local_codecs)
            .build();
        let local_offer = local_track
            .local_description()
            .await
            .expect("local offer should be created");
        peer.update_track(Box::new(local_track), None).await;

        let remote_answer_track = RtpTrackBuilder::new(format!("{}-remote-answer", call_id))
            .with_codec_preference(remote_codecs)
            .build();
        let remote_answer = remote_answer_track
            .handshake(local_offer.clone())
            .await
            .expect("remote answer should be created");
        peer.update_remote_description(&track_id, &remote_answer)
            .await
            .expect("initial remote answer should apply");

        let leg = RwiCallLeg::new_outbound(
            call_id.to_string(),
            tx,
            peer.clone(),
            Some(local_offer),
            CancellationToken::new(),
            Some("1001".to_string()),
            Some("2001".to_string()),
        );
        leg.set_answer(remote_answer.clone()).await;
        leg.set_negotiated_media(
            RwiCommandProcessor::select_best_audio_from_sdp(&remote_answer),
            MediaNegotiator::extract_ssrc(&remote_answer),
        )
        .await;
        leg.set_connected_dialog_id(format!("dlg-{}", call_id)).await;
        leg.set_state(crate::rwi::call_leg::RwiCallLegState::Answered)
            .await;
        processor
            .gateway
            .write()
            .await
            .register_leg(call_id.to_string(), leg.clone());

        (leg, peer, track_id)
    }

    async fn build_test_remote_offer(track_id: &str, codecs: Vec<CodecType>) -> String {
        RtpTrackBuilder::new(track_id.to_string())
            .with_codec_preference(codecs)
            .build()
            .local_description()
            .await
            .expect("remote offer should be created")
    }

    #[tokio::test]
    async fn test_list_calls_empty() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert!(calls.is_empty());
        }
    }

    #[tokio::test]
    async fn test_answer_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_ring_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Ring {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_reject_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Reject {
                call_id: "nonexistent".into(),
                reason: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_attach_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::AttachCall {
                call_id: "nonexistent".into(),
                mode: crate::rwi::session::OwnershipMode::Control,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-to-detach",
            "caller1",
            "callee1",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry.clone());

        // Detach should succeed
        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "call-to-detach".into(),
            })
            .await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), CommandResult::Success));

        // The call should still exist in registry (DetachCall doesn't remove it)
        assert!(registry.get_handle("call-to-detach").is_some());
    }

    #[tokio::test]
    async fn test_hangup_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "nonexistent".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_transfer_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Transfer {
                call_id: "nonexistent".into(),
                target: "sip:target@local".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_a() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "missing-a".into(),
                leg_b: "missing-b".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_b() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b-missing".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_both_media_ready_legs_succeeds() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let ha =
            create_test_media_ready_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let hb =
            create_test_media_ready_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);
        register_test_media_ready_rwi_leg(&processor, "leg-a", ha).await;
        register_test_media_ready_rwi_leg(&processor, "leg-b", hb).await;

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;
        assert!(result.is_ok(), "bridge should succeed for media-ready legs");
    }

    #[tokio::test]
    async fn test_bridge_attached_and_standalone_media_ready_legs_succeeds() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let attached =
            create_test_media_ready_call(&registry, "leg-attached", "1001", "2001", DialDirection::Inbound);
        register_test_media_ready_rwi_leg(&processor, "leg-attached", attached).await;
        register_test_standalone_media_ready_rwi_leg(&processor, "leg-standalone").await;

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-attached".into(),
                leg_b: "leg-standalone".into(),
            })
            .await;
        assert!(result.is_ok(), "bridge should succeed for attached + standalone legs");
    }

    #[tokio::test]
    async fn test_bridge_standalone_media_ready_legs_succeeds() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "leg-standalone-a").await;
        register_test_standalone_media_ready_rwi_leg(&processor, "leg-standalone-b").await;

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-standalone-a".into(),
                leg_b: "leg-standalone-b".into(),
            })
            .await;
        assert!(result.is_ok(), "bridge should succeed for standalone legs");
    }

    #[tokio::test]
    async fn test_unbridge_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "nope".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_subscribe_success() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Subscribe {
                contexts: vec!["ctx1".into()],
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unsubscribe_success() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unsubscribe {
                contexts: vec!["ctx1".into()],
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_play_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaPlay(
                crate::rwi::session::MediaPlayRequest {
                    call_id: "missing".into(),
                    source: crate::rwi::session::MediaSource {
                        source_type: "file".into(),
                        uri: Some("welcome.wav".into()),
                        looped: None,
                    },
                    interrupt_on_dtmf: false,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_originate_no_server_returns_error() {
        // Without a SIP server wired in, originate should return CommandFailed
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                crate::rwi::session::OriginateRequest {
                    call_id: "new-call".into(),
                    destination: "sip:test@local".into(),
                    caller_id: None,
                    timeout_secs: Some(30),
                    hold_music: None,
                    hold_music_target: None,
                    ringback: None,
                    ringback_target: None,
                    extra_headers: std::collections::HashMap::new(),
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_originate_invalid_destination_returns_error() {
        // Even with a SIP server, an invalid URI should return CommandFailed
        // Here we don't have a real SipServer so we just verify the no-server path;
        // URI-parse errors are caught in the same method after the server check.
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                crate::rwi::session::OriginateRequest {
                    call_id: "new-call-2".into(),
                    destination: "not-a-sip-uri".into(),
                    caller_id: None,
                    timeout_secs: None,
                    hold_music: None,
                    hold_music_target: None,
                    ringback: None,
                    ringback_target: None,
                    extra_headers: std::collections::HashMap::new(),
                },
            ))
            .await;
        assert!(result.is_err());
        // No server → "SIP server not available" wins before URI parse
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    // ===== Integration: calls with real handles =====

    #[tokio::test]
    async fn test_answer_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        assert!(registry.get_handle("call-001").is_some());

        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "call-001".into(),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_hangup_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "call-001".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_list_calls_with_multiple_calls() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());

        create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Outbound);
        create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert_eq!(calls.len(), 3);
            let ids: Vec<_> = calls.iter().map(|c| c.session_id.clone()).collect();
            assert!(ids.contains(&"call-1".to_string()));
            assert!(ids.contains(&"call-2".to_string()));
            assert!(ids.contains(&"call-3".to_string()));
        }
    }

    #[tokio::test]
    async fn test_list_calls_includes_gateway_only_originated_leg() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry);
        let (tx, _rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("standalone-list-leg".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "standalone-leg".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("1001".to_string()),
            Some("2000".to_string()),
        );
        processor
            .gateway
            .write()
            .await
            .register_leg("standalone-leg".to_string(), leg);

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert!(
                calls.iter().any(|call| call.session_id == "standalone-leg"),
                "standalone originated leg should be listed without a proxy session handle"
            );
        }
    }

    #[tokio::test]
    async fn test_call_direction_filtering() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());

        create_test_call(
            &registry,
            "inbound-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        create_test_call(
            &registry,
            "outbound-1",
            "2001",
            "1001",
            DialDirection::Outbound,
        );
        create_test_call(
            &registry,
            "inbound-2",
            "1002",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        if let Ok(CommandResult::ListCalls(calls)) = result {
            let inbound: Vec<_> = calls.iter().filter(|c| c.direction == "inbound").collect();
            let outbound: Vec<_> = calls.iter().filter(|c| c.direction == "outbound").collect();
            assert_eq!(inbound.len(), 2);
            assert_eq!(outbound.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_bridge_emits_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway.clone()));

        // Create two legs
        let ha =
            create_test_media_ready_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let hb =
            create_test_media_ready_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);
        register_test_media_ready_rwi_leg(&processor, "leg-a", ha).await;
        register_test_media_ready_rwi_leg(&processor, "leg-b", hb).await;

        // Register a session in gateway and claim ownership of leg-a
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write().await;
            let identity = crate::rwi::auth::RwiIdentity {
                token: "t".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "leg-a".into(),
                crate::rwi::session::OwnershipMode::Control,
            )
            .await
            .unwrap();
        }

        // Bridge should emit CallBridged to owner of leg-a
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        match result {
            Ok(_) => {
                let ev = event_rx.recv().await;
                assert!(ev.is_some(), "Expected CallBridged event on gateway");
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    // ── StopPlayback / Unbridge command paths ──────────────────────────────

    #[tokio::test]
    async fn test_media_stop_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "ghost".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stop_existing_call_sends_stop_playback() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-stop",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "call-stop".into(),
            })
            .await;
        // Success OR CommandFailed (channel closed) — NOT not_found
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("StopPlayback should be queued");
        assert_eq!(
            cmd,
            crate::proxy::proxy_call::state::SessionAction::StopPlayback
        );
    }

    #[tokio::test]
    async fn test_unbridge_existing_call_does_not_send_session_unbridge() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unb",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-unb".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        assert!(
            rx.try_recv().is_err(),
            "unbridge should no longer fall back to SessionAction::Unbridge"
        );
    }

    #[tokio::test]
    async fn test_bridge_media_ready_calls_starts_direct_bridge() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let ha =
            create_test_media_ready_call(&registry, "leg-a2", "1001", "2001", DialDirection::Outbound);
        let hb =
            create_test_media_ready_call(&registry, "leg-b2", "1001", "2002", DialDirection::Outbound);
        register_test_media_ready_rwi_leg(&processor, "leg-a2", ha).await;
        register_test_media_ready_rwi_leg(&processor, "leg-b2", hb).await;

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a2".into(),
                leg_b: "leg-b2".into(),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let gw = processor.gateway.read().await;
        assert!(gw.bridge_id_for_leg(&"leg-a2".to_string()).is_some());
        assert!(gw.bridge_id_for_leg(&"leg-b2".to_string()).is_some());
    }

    #[tokio::test]
    async fn test_attach_call_registers_attached_rwi_leg_with_media() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let handle = create_test_media_ready_call(
            &registry,
            "attached-leg",
            "1001",
            "2001",
            DialDirection::Inbound,
        );
        publish_test_media_to_handle(&handle, "attached-leg");
        let leg = RwiCallLeg::new_attached(
            &crate::proxy::active_call_registry::ActiveProxyCallEntry {
                session_id: "attached-leg".to_string(),
                caller: Some("1001".to_string()),
                callee: Some("2001".to_string()),
                direction: "inbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: Some(chrono::Utc::now()),
                status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking,
            },
            handle,
        );
        processor
            .gateway
            .write()
            .await
            .register_leg("attached-leg".to_string(), leg);

        let result = processor
            .process_command(RwiCommandPayload::AttachCall {
                call_id: "attached-leg".into(),
                mode: crate::rwi::session::OwnershipMode::Control,
            })
            .await;
        assert!(result.is_ok());

        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"attached-leg".to_string())
            .expect("attached leg should be registered in RWI");
        let live_media = leg
            .live_media()
            .await
            .expect("attached leg should expose live media");
        assert_eq!(live_media.negotiated_audio.0, CodecType::PCMU);
    }

    #[tokio::test]
    async fn test_answer_standalone_originated_leg_returns_unsupported() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-answer").await;

        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "standalone-answer".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported for standalone originated RWI leg"));
    }

    #[tokio::test]
    async fn test_ring_standalone_originated_leg_returns_unsupported() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-ring").await;

        let result = processor
            .process_command(RwiCommandPayload::Ring {
                call_id: "standalone-ring".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported for standalone originated RWI leg"));
    }

    #[tokio::test]
    async fn test_media_play_standalone_originated_leg_returns_track_id() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-media-play").await;

        let result = processor
            .process_command(RwiCommandPayload::MediaPlay(
                crate::rwi::session::MediaPlayRequest {
                    call_id: "standalone-media-play".into(),
                    source: crate::rwi::session::MediaSource {
                        source_type: "silence".into(),
                        uri: None,
                        looped: None,
                    },
                    interrupt_on_dtmf: false,
                },
            ))
            .await;
        match result {
            Ok(CommandResult::MediaPlay { track_id }) => assert!(!track_id.is_empty()),
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_transfer_standalone_originated_leg_queues_transfer_command() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("standalone-transfer-stream".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "standalone-transfer".to_string(),
            tx,
            peer,
            Some("v=0".to_string()),
            CancellationToken::new(),
            Some("1001".to_string()),
            Some("2001".to_string()),
        );
        leg.set_negotiated_media(
            Some((
                CodecType::PCMU,
                rustrtc::RtpCodecParameters {
                    payload_type: CodecType::PCMU.payload_type(),
                    clock_rate: CodecType::PCMU.clock_rate(),
                    channels: CodecType::PCMU.channels() as u8,
                },
                Vec::new(),
            )),
            Some(1234),
        )
        .await;
        leg.set_state(crate::rwi::call_leg::RwiCallLegState::Answered)
            .await;
        processor
            .gateway
            .write()
            .await
            .register_leg("standalone-transfer".to_string(), leg);

        let result = processor
            .process_command(RwiCommandPayload::Transfer {
                call_id: "standalone-transfer".into(),
                target: "sip:target@local".into(),
            })
            .await;
        assert!(matches!(result, Ok(CommandResult::Success)));
        let cmd = rx.recv().await.expect("transfer command should be queued");
        assert!(
            matches!(cmd, LegCommand::Transfer { ref target } if target == "sip:target@local"),
            "expected Transfer command, got {:?}",
            cmd
        );
    }

    #[tokio::test]
    async fn test_transfer_standalone_originated_leg_returns_error_when_negotiation_in_progress() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-transfer-pending")
            .await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-transfer-pending".to_string())
            .expect("standalone leg should be registered");
        leg.try_begin_negotiation("INVITE")
            .await
            .expect("negotiation should start");

        let result = processor
            .process_command(RwiCommandPayload::Transfer {
                call_id: "standalone-transfer-pending".into(),
                target: "sip:target@local".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("standalone renegotiation already in progress"));
    }

    #[tokio::test]
    async fn test_hold_standalone_originated_leg_succeeds() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-hold").await;

        let result = processor
            .process_command(RwiCommandPayload::CallHold {
                call_id: "standalone-hold".into(),
                music: None,
            })
            .await;
        assert!(matches!(result, Ok(CommandResult::Success)));
    }

    #[tokio::test]
    async fn test_hold_standalone_originated_leg_returns_error_when_negotiation_in_progress() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-hold-pending").await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-hold-pending".to_string())
            .expect("leg should exist");
        leg.try_begin_negotiation("INVITE")
            .await
            .expect("negotiation should start");

        let result = processor
            .process_command(RwiCommandPayload::CallHold {
                call_id: "standalone-hold-pending".into(),
                music: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("standalone renegotiation already in progress"));
    }

    #[tokio::test]
    async fn test_unhold_standalone_originated_leg_succeeds() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-unhold").await;

        let result = processor
            .process_command(RwiCommandPayload::CallUnhold {
                call_id: "standalone-unhold".into(),
            })
            .await;
        assert!(matches!(result, Ok(CommandResult::Success)));
    }

    #[tokio::test]
    async fn test_unhold_standalone_originated_leg_returns_error_when_negotiation_in_progress() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-unhold-pending")
            .await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-unhold-pending".to_string())
            .expect("leg should exist");
        leg.try_begin_negotiation("UPDATE")
            .await
            .expect("negotiation should start");

        let result = processor
            .process_command(RwiCommandPayload::CallUnhold {
                call_id: "standalone-unhold-pending".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("standalone renegotiation already in progress"));
    }

    #[tokio::test]
    async fn test_media_stop_standalone_originated_leg_succeeds() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-media-stop").await;

        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "standalone-media-stop".into(),
            })
            .await;
        assert!(matches!(result, Ok(CommandResult::Success)));
    }

    #[tokio::test]
    async fn test_record_start_standalone_originated_leg_returns_unsupported() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-record").await;

        let result = processor
            .process_command(RwiCommandPayload::RecordStart(RecordStartRequest {
                call_id: "standalone-record".into(),
                mode: "mixed".into(),
                storage: crate::rwi::session::RecordStorage {
                    path: "/recordings/test.wav".into(),
                    backend: "file".into(),
                },
                max_duration_secs: None,
                beep: None,
            }))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported for standalone originated RWI leg"));
    }

    #[tokio::test]
    async fn test_queue_enqueue_standalone_originated_leg_returns_unsupported() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-queue").await;

        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "standalone-queue".into(),
                queue_id: "support".into(),
                priority: None,
                skills: Some(vec![]),
                max_wait_secs: None,
            }))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported for standalone originated RWI leg"));
    }

    #[tokio::test]
    async fn test_unbridge_direct_bridge_clears_rwi_bridge_registry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let ha =
            create_test_media_ready_call(&registry, "leg-a3", "1001", "2001", DialDirection::Outbound);
        let hb =
            create_test_media_ready_call(&registry, "leg-b3", "1001", "2002", DialDirection::Outbound);
        register_test_media_ready_rwi_leg(&processor, "leg-a3", ha).await;
        register_test_media_ready_rwi_leg(&processor, "leg-b3", hb).await;

        processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a3".into(),
                leg_b: "leg-b3".into(),
            })
            .await
            .expect("direct bridge should succeed");
        processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "leg-a3".into(),
            })
            .await
            .expect("unbridge should succeed");

        let gw = processor.gateway.read().await;
        assert!(gw.bridge_id_for_leg(&"leg-a3".to_string()).is_none());
        assert!(gw.bridge_id_for_leg(&"leg-b3".to_string()).is_none());
        assert_eq!(gw.bridge_count(), 0);
    }

    #[tokio::test]
    async fn test_standalone_remote_update_with_sdp_updates_negotiated_media() {
        let processor = create_test_processor();
        let (leg, peer, track_id) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-update",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;
        let remote_offer =
            build_test_remote_offer("remote-update-offer", vec![CodecType::G722]).await;

        RwiCommandProcessor::apply_standalone_remote_offer(
            processor.gateway.clone(),
            processor.sip_server.clone(),
            "standalone-update",
            &leg,
            peer,
            &track_id,
            &remote_offer,
        )
        .await
        .expect("standalone update should succeed");

        let live_media = leg.live_media().await.expect("leg should still be live");
        assert_eq!(live_media.negotiated_audio.0, CodecType::G722);
    }

    #[tokio::test]
    async fn test_standalone_remote_reinvite_with_sdp_updates_negotiated_media() {
        let processor = create_test_processor();
        let (leg, peer, track_id) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-reinvite",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;
        let remote_offer =
            build_test_remote_offer("remote-reinvite-offer", vec![CodecType::G722]).await;

        let local_answer = RwiCommandProcessor::apply_standalone_remote_offer(
            processor.gateway.clone(),
            processor.sip_server.clone(),
            "standalone-reinvite",
            &leg,
            peer,
            &track_id,
            &remote_offer,
        )
        .await
        .expect("standalone re-invite should succeed");

        assert!(local_answer.contains("m=audio"));
        let live_media = leg.live_media().await.expect("leg should still be live");
        assert_eq!(live_media.negotiated_audio.0, CodecType::G722);
    }

    #[tokio::test]
    async fn test_invalid_remote_sdp_on_standalone_leg_preserves_prior_media_state() {
        let processor = create_test_processor();
        let (leg, peer, track_id) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-invalid-update",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;
        let before = leg.live_media().await.expect("leg should be live before failure");

        let result = RwiCommandProcessor::apply_standalone_remote_offer(
            processor.gateway.clone(),
            processor.sip_server.clone(),
            "standalone-invalid-update",
            &leg,
            peer,
            &track_id,
            "invalid sdp",
        )
        .await;

        assert!(result.is_err());
        let after = leg.live_media().await.expect("leg should still be live after failure");
        assert_eq!(after.negotiated_audio.0, before.negotiated_audio.0);
        assert_eq!(after.ssrc, before.ssrc);
    }

    #[tokio::test]
    async fn test_bridged_standalone_media_update_rebuilds_direct_bridge() {
        let processor = create_test_processor();
        let (leg_a, peer_a, track_id_a) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-bridge-a",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;
        let (_leg_b, _peer_b, _track_id_b) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-bridge-b",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;

        processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "standalone-bridge-a".into(),
                leg_b: "standalone-bridge-b".into(),
            })
            .await
            .expect("initial bridge should succeed");

        let remote_offer =
            build_test_remote_offer("remote-bridge-offer", vec![CodecType::G722]).await;
        RwiCommandProcessor::apply_standalone_remote_offer(
            processor.gateway.clone(),
            processor.sip_server.clone(),
            "standalone-bridge-a",
            &leg_a,
            peer_a,
            &track_id_a,
            &remote_offer,
        )
        .await
        .expect("media refresh should rebuild bridge");

        let gw = processor.gateway.read().await;
        assert_eq!(gw.bridge_count(), 1);
        assert!(gw.bridge_id_for_leg(&"standalone-bridge-a".to_string()).is_some());
        assert!(gw.bridge_id_for_leg(&"standalone-bridge-b".to_string()).is_some());
        let live_media = leg_a.live_media().await.expect("leg should remain bridged");
        assert_eq!(live_media.negotiated_audio.0, CodecType::G722);
    }

    #[tokio::test]
    async fn test_unbridged_standalone_media_update_does_not_create_bridge_state() {
        let processor = create_test_processor();
        let (leg, peer, track_id) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-unbridged-update",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;
        let remote_offer =
            build_test_remote_offer("remote-unbridged-offer", vec![CodecType::G722]).await;

        RwiCommandProcessor::apply_standalone_remote_offer(
            processor.gateway.clone(),
            processor.sip_server.clone(),
            "standalone-unbridged-update",
            &leg,
            peer,
            &track_id,
            &remote_offer,
        )
        .await
        .expect("media refresh should succeed without bridge");

        let gw = processor.gateway.read().await;
        assert_eq!(gw.bridge_count(), 0);
        assert!(gw.bridge_id_for_leg(&"standalone-unbridged-update".to_string()).is_none());
    }

    #[tokio::test]
    async fn test_rewrite_audio_direction_in_sdp_for_hold() {
        let offer = build_test_remote_offer("local-hold-offer", vec![CodecType::PCMU]).await;

        let rewritten =
            RwiCommandProcessor::rewrite_audio_direction_in_sdp(&offer, "sendonly").unwrap();
        let parsed = SessionDescription::parse(SdpType::Offer, &rewritten).unwrap();
        let audio = parsed
            .media_sections
            .iter()
            .find(|section| section.kind == MediaKind::Audio)
            .unwrap();
        assert_eq!(audio.direction, Direction::SendOnly);
    }

    #[tokio::test]
    async fn test_build_standalone_local_offer_rewrites_direction() {
        let processor = create_test_processor();
        let (_leg, peer, track_id) = register_test_standalone_rwi_leg_with_real_track(
            &processor,
            "standalone-local-offer",
            vec![CodecType::PCMU, CodecType::G722],
            vec![CodecType::PCMU],
        )
        .await;

        let offer = RwiCommandProcessor::build_standalone_local_offer(
            peer,
            &track_id,
            Some("sendonly"),
        )
        .await
        .expect("offer should be built");

        assert!(offer.contains("a=sendonly"));
        assert!(!offer.contains("a=inactive"));
    }

    #[tokio::test]
    async fn test_standalone_session_timer_initializes_from_200_ok() {
        let response = rsip::Response::try_from(
            "SIP/2.0 200 OK\r\nSession-Expires: 180;refresher=uas\r\n\r\n",
        )
        .expect("response should parse");

        let timer = RwiCommandProcessor::build_standalone_session_timer(true, &response, 90)
            .expect("timer should be initialized");

        assert!(timer.enabled);
        assert!(timer.active);
        assert_eq!(timer.session_interval.as_secs(), 180);
        assert_eq!(timer.refresher, SessionRefresher::Uas);
    }

    #[tokio::test]
    async fn test_standalone_session_timer_uses_default_when_header_missing() {
        let response = rsip::Response::try_from("SIP/2.0 200 OK\r\n\r\n")
            .expect("response should parse");

        let timer = RwiCommandProcessor::build_standalone_session_timer(true, &response, 600)
            .expect("timer should be initialized");

        assert_eq!(timer.session_interval.as_secs(), 600);
        assert_eq!(timer.refresher, SessionRefresher::Uac);
    }

    #[tokio::test]
    async fn test_standalone_session_timer_tick_does_not_refresh_when_not_refresher() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-timer-uas").await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-timer-uas".to_string())
            .expect("leg should exist");

        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uas;
        timer.last_refresh = Instant::now() - (timer.session_interval / 2);
        leg.set_session_timer(Some(timer)).await;

        let should_break = RwiCommandProcessor::handle_standalone_session_timer_tick(
            "standalone-timer-uas",
            &leg,
            None,
            processor.gateway.clone(),
        )
        .await
        .expect("tick should succeed");

        assert!(!should_break);
        let timer = leg.session_timer().await.expect("timer should still exist");
        assert!(!timer.refreshing);
    }

    #[tokio::test]
    async fn test_standalone_session_timer_tick_skips_when_negotiation_in_progress() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-timer-pending")
            .await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-timer-pending".to_string())
            .expect("leg should exist");

        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uac;
        timer.last_refresh = Instant::now() - (timer.session_interval / 2);
        leg.set_session_timer(Some(timer)).await;
        leg.try_begin_negotiation("INVITE")
            .await
            .expect("negotiation should start");

        let should_break = RwiCommandProcessor::handle_standalone_session_timer_tick(
            "standalone-timer-pending",
            &leg,
            None,
            processor.gateway.clone(),
        )
        .await
        .expect("tick should succeed");

        assert!(!should_break);
        let timer = leg.session_timer().await.expect("timer should still exist");
        assert!(!timer.refreshing);
        assert_eq!(leg.pending_negotiation_method().await.as_deref(), Some("INVITE"));
    }

    #[tokio::test]
    async fn test_standalone_session_timer_expiry_terminates_leg() {
        let processor = create_test_processor();
        register_test_standalone_media_ready_rwi_leg(&processor, "standalone-timer-expired").await;
        let leg = processor
            .gateway
            .read()
            .await
            .get_leg(&"standalone-timer-expired".to_string())
            .expect("leg should exist");

        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uac;
        timer.last_refresh = Instant::now() - timer.session_interval - std::time::Duration::from_secs(1);
        leg.set_session_timer(Some(timer)).await;

        let should_break = RwiCommandProcessor::handle_standalone_session_timer_tick(
            "standalone-timer-expired",
            &leg,
            None,
            processor.gateway.clone(),
        )
        .await
        .expect("tick should succeed");

        assert!(should_break);
        assert_eq!(leg.state().await, RwiCallLegState::Terminated);
    }

    #[tokio::test]
    async fn test_unbridge_emits_call_unbridged_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway.clone()));

        let (_handle, _rx) =
            create_test_call_with_rx(&registry, "call-ev", "1001", "2000", DialDirection::Inbound);

        // Register a session and wire an event channel
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write().await;
            let identity = crate::rwi::auth::RwiIdentity {
                token: "t2".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "call-ev".into(),
                crate::rwi::session::OwnershipMode::Control,
            )
            .await
            .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-ev".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                let ev = event_rx.recv().await;
                assert!(ev.is_some(), "Expected CallUnbridged event");
                let v = ev.unwrap();
                // The event JSON should contain the call_id
                let s = serde_json::to_string(&v).unwrap();
                assert!(s.contains("call-ev"), "Event should reference call-ev");
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_set_ringback_source_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 = create_test_call(
            &registry,
            "call-source",
            "1002",
            "2001",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_set_ringback_source_target_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-source",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "nonexistent".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_set_ringback_source_source_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_record_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/call-rec.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
        if let Ok(action) = cmd {
            assert!(matches!(action, SessionAction::StartRecording { .. }));
        }
    }

    #[tokio::test]
    async fn test_record_start_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "nonexistent".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/call.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_record_pause_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-p",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-p".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-p".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_pause_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-norec".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_resume_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-r",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-r".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-r".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-rec-r".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_resume_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-norec2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-s",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-s".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-rec-s".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let mut found_stop = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, SessionAction::StopRecording) {
                found_stop = true;
                break;
            }
        }
        assert!(found_stop, "Expected StopRecording action to be sent");
    }

    #[tokio::test]
    async fn test_record_stop_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-norec3",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-norec3".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        if let Ok(action) = cmd {
            assert!(matches!(action, SessionAction::StopRecording));
        }
    }

    #[tokio::test]
    async fn test_queue_enqueue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-q",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-q".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await;
        assert!(result.is_ok());

        assert_eq!(handle.queue_name(), Some("support".to_string()));
    }

    #[tokio::test]
    async fn test_queue_enqueue_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "nonexistent".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_dequeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-dq",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-dq".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "call-dq".into(),
            })
            .await;
        assert!(result.is_ok());

        assert_eq!(handle.queue_name(), None);
    }

    #[tokio::test]
    async fn test_queue_dequeue_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_hold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-hold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-hold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-hold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_hold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-noq".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_queue_unhold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unhold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-unhold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-unhold".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-unhold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_unhold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-noq2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_supervisor_listen_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_listen_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "nonexistent".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_supervisor_whisper_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "agent-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_barge_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "agent-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorStop {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_stream_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "call-1".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_stream_start_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "nonexistent".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stream_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "call-1".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStop {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_inject_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::MediaInjectStart(
                crate::rwi::session::MediaInjectRequest {
                    call_id: "call-1".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_inject_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::MediaInjectStart(
                crate::rwi::session::MediaInjectRequest {
                    call_id: "call-1".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::MediaInjectStop {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sip_message_no_server() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_notify_no_server() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "check-sync".into(),
                content_type: "application/simple-message-summary".into(),
                body: "".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_options_ping_no_server() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    // Conference tests
    #[tokio::test]
    async fn test_conference_create_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: Some(10),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceCreated { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceCreated result"),
        }
    }

    #[tokio::test]
    async fn test_conference_create_duplicate_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create first conference
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        // Try to create duplicate
        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_conference_create_external_requires_mcu_uri() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "external".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("external backend requires mcu_uri")
        );
    }

    #[tokio::test]
    async fn test_conference_add_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"));
    }

    #[tokio::test]
    async fn test_conference_destroy_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create conference
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        // Destroy it
        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "room-1".into(),
            })
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceDestroyed { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceDestroyed result"),
        }
    }

    #[tokio::test]
    async fn test_conference_destroy_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_conference_mute_not_in_conference_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create conference but don't add any calls
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        // Try to mute a call that's not in the conference
        let result = processor
            .process_command(RwiCommandPayload::ConferenceMute {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("is not in conference")
        );
    }

    #[tokio::test]
    async fn test_conference_add_with_max_members() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create conference with max 2 members
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: Some(2),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        // Create and add first call
        let _handle1 =
            create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());

        // Create and add second call
        let _handle2 =
            create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-2".into(),
            })
            .await;
        assert!(result.is_ok());

        // Try to add third call - should fail
        let _handle3 =
            create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-3".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("is full"));
    }

    // Queue new commands tests
    #[tokio::test]
    async fn test_queue_set_priority_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        // Set priority
        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_set_priority_not_in_queue_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call but don't enqueue it
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        // Try to set priority - should fail
        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in queue"));
    }

    #[tokio::test]
    async fn test_queue_assign_agent_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        // Assign agent
        let result = processor
            .process_command(RwiCommandPayload::QueueAssignAgent {
                call_id: "call-1".into(),
                agent_id: "agent-42".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_requeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        // Requeue to different queue
        let result = processor
            .process_command(RwiCommandPayload::QueueRequeue {
                call_id: "call-1".into(),
                queue_id: "sales".into(),
                priority: Some(5),
            })
            .await;
        assert!(result.is_ok());
    }

    // Record mask segment test
    #[tokio::test]
    async fn test_record_mask_segment_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and manually add to record_states (bypassing the actual recording command)
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        {
            let mut states = processor.record_states.write().await;
            states.insert(
                "call-1".to_string(),
                RecordState {
                    recording_id: "rec-1".to_string(),
                    _mode: "mixed".to_string(),
                    _path: "/tmp/recording.wav".to_string(),
                    is_paused: false,
                },
            );
        }

        // Mask a segment
        let result = processor
            .process_command(RwiCommandPayload::RecordMaskSegment {
                call_id: "call-1".into(),
                recording_id: "rec-1".into(),
                start_secs: 30,
                end_secs: 60,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_record_mask_segment_no_recording_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call but don't start recording
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        // Try to mask segment - should fail
        let result = processor
            .process_command(RwiCommandPayload::RecordMaskSegment {
                call_id: "call-1".into(),
                recording_id: "rec-1".into(),
                start_secs: 30,
                end_secs: 60,
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No active recording")
        );
    }
}
