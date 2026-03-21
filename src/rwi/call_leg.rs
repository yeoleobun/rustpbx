use crate::media::negotiate::CodecInfo;
use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
use crate::proxy::proxy_call::media_peer::MediaPeer;
use crate::proxy::proxy_call::session_timer::{
    HEADER_SESSION_EXPIRES, SessionRefresher, SessionTimerState, parse_session_expires,
};
use crate::proxy::proxy_call::state::{CallSessionHandle, SessionAction, SharedCallerMediaRef};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RwiCallLegOrigin {
    InboundAttached,
    OutboundOriginated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RwiCallLegState {
    Initializing,
    Ringing,
    EarlyMedia,
    Answered,
    Bridged,
    Terminated,
    Failed,
}

#[derive(Clone)]
pub(crate) struct RwiLiveLegMedia {
    pub peer: Arc<dyn MediaPeer>,
    pub negotiated_audio: (CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>),
    pub ssrc: Option<u32>,
}

#[derive(Clone)]
pub(crate) struct RwiCallLegRuntime {
    pub peer: Option<Arc<dyn MediaPeer>>,
    #[allow(dead_code)]
    pub offer_sdp: Option<String>,
    pub answer_sdp: Option<String>,
    pub negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    pub dialog_ids: HashSet<String>,
    pub connected_dialog_id: Option<String>,
    pub ssrc: Option<u32>,
    pub state: RwiCallLegState,
    pub session_timer: Option<SessionTimerState>,
    pub negotiation_in_flight: bool,
    pub pending_method: Option<String>,
}

impl Default for RwiCallLegRuntime {
    fn default() -> Self {
        Self {
            peer: None,
            offer_sdp: None,
            answer_sdp: None,
            negotiated_audio: None,
            dialog_ids: HashSet::new(),
            connected_dialog_id: None,
            ssrc: None,
            state: RwiCallLegState::Initializing,
            session_timer: None,
            negotiation_in_flight: false,
            pending_method: None,
        }
    }
}

pub(crate) use crate::proxy::proxy_call::leg_command::LegCommand;

#[derive(Clone)]
pub(crate) enum RwiLegCommandHandle {
    Session(CallSessionHandle),
    Standalone(mpsc::UnboundedSender<LegCommand>),
}

impl RwiLegCommandHandle {
    pub(crate) fn send_action(&self, action: SessionAction) -> Result<()> {
        match self {
            Self::Session(handle) => handle.send_command(action),
            Self::Standalone(tx) => {
                let cmd = match action {
                    SessionAction::Hangup {
                        reason,
                        code,
                        initiator,
                    } => LegCommand::Hangup {
                        reason,
                        code,
                        initiator,
                    },
                    SessionAction::Hold { music_source } => LegCommand::Hold { music_source },
                    SessionAction::Unhold => LegCommand::Unhold,
                    SessionAction::PlayPrompt {
                        audio_file,
                        track_id,
                        loop_playback,
                        ..
                    } => LegCommand::PlayAudio {
                        file: audio_file,
                        track_id: track_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        loop_playback,
                    },
                    SessionAction::StopPlayback => LegCommand::StopPlayback,
                    _ => {
                        return Err(anyhow!(
                            "command unsupported for standalone originated RWI leg"
                        ))
                    }
                };
                tx.send(cmd).map_err(|e| anyhow!(e.to_string()))
            }
        }
    }

    pub(crate) fn session_handle(&self) -> Option<CallSessionHandle> {
        match self {
            Self::Session(handle) => Some(handle.clone()),
            Self::Standalone(_) => None,
        }
    }

    pub(crate) fn send_transfer(&self, target: String) -> Result<()> {
        match self {
            Self::Session(handle) => handle.send_command(SessionAction::from_transfer_target(&target)),
            Self::Standalone(tx) => tx
                .send(LegCommand::Transfer { target })
                .map_err(|e| anyhow!(e.to_string())),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RwiCallLegInfo {
    pub call_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub status: ActiveProxyCallStatus,
}

pub(crate) struct RwiCallLeg {
    #[allow(dead_code)]
    call_id: String,
    origin: RwiCallLegOrigin,
    command_handle: RwiLegCommandHandle,
    #[allow(dead_code)]
    cancel_token: Option<CancellationToken>,
    info: RwLock<RwiCallLegInfo>,
    runtime: RwLock<RwiCallLegRuntime>,
    /// For attached legs: shared media state published by the session's caller leg.
    /// Reads come from here instead of `runtime` for media fields.
    shared_media: Option<SharedCallerMediaRef>,
}

pub(crate) type RwiCallLegHandle = Arc<RwiCallLeg>;

impl RwiCallLeg {
    pub(crate) fn new_attached(
        entry: &ActiveProxyCallEntry,
        owner_handle: CallSessionHandle,
    ) -> RwiCallLegHandle {
        let shared_media = Some(owner_handle.shared_caller_media());
        Arc::new(Self {
            call_id: entry.session_id.clone(),
            origin: RwiCallLegOrigin::InboundAttached,
            command_handle: RwiLegCommandHandle::Session(owner_handle),
            cancel_token: None,
            info: RwLock::new(RwiCallLegInfo {
                call_id: entry.session_id.clone(),
                caller: entry.caller.clone(),
                callee: entry.callee.clone(),
                direction: entry.direction.clone(),
                started_at: entry.started_at,
                answered_at: entry.answered_at,
                status: entry.status,
            }),
            runtime: RwLock::new(RwiCallLegRuntime::default()),
            shared_media,
        })
    }

    pub(crate) fn new_outbound(
        call_id: String,
        command_handle: mpsc::UnboundedSender<LegCommand>,
        peer: Arc<dyn MediaPeer>,
        offer_sdp: Option<String>,
        cancel_token: CancellationToken,
        caller: Option<String>,
        callee: Option<String>,
    ) -> RwiCallLegHandle {
        Arc::new(Self {
            call_id: call_id.clone(),
            origin: RwiCallLegOrigin::OutboundOriginated,
            command_handle: RwiLegCommandHandle::Standalone(command_handle),
            cancel_token: Some(cancel_token),
            info: RwLock::new(RwiCallLegInfo {
                call_id,
                caller,
                callee,
                direction: "outbound".to_string(),
                started_at: Utc::now(),
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            }),
            runtime: RwLock::new(RwiCallLegRuntime {
                peer: Some(peer),
                offer_sdp,
                answer_sdp: None,
                negotiated_audio: None,
                dialog_ids: HashSet::new(),
                connected_dialog_id: None,
                ssrc: None,
                state: RwiCallLegState::Initializing,
                session_timer: None,
                negotiation_in_flight: false,
                pending_method: None,
            }),
            shared_media: None,
        })
    }

    pub(crate) fn origin(&self) -> RwiCallLegOrigin {
        self.origin
    }

    pub(crate) fn command_handle(&self) -> &RwiLegCommandHandle {
        &self.command_handle
    }

    pub(crate) fn session_handle(&self) -> Option<CallSessionHandle> {
        self.command_handle.clone()
            .session_handle()
    }

    pub(crate) fn supports_session_features(&self) -> bool {
        matches!(self.command_handle, RwiLegCommandHandle::Session(_))
    }

    pub(crate) async fn set_state(&self, state: RwiCallLegState) {
        {
            let mut runtime = self.runtime.write().await;
            runtime.state = state;
        }
        let mut info = self.info.write().await;
        info.status = match state {
            RwiCallLegState::Initializing
            | RwiCallLegState::Ringing
            | RwiCallLegState::EarlyMedia => ActiveProxyCallStatus::Ringing,
            RwiCallLegState::Answered | RwiCallLegState::Bridged => ActiveProxyCallStatus::Talking,
            RwiCallLegState::Terminated | RwiCallLegState::Failed => info.status,
        };
        if matches!(state, RwiCallLegState::Answered | RwiCallLegState::Bridged)
            && info.answered_at.is_none()
        {
            info.answered_at = Some(Utc::now());
        }
    }

    pub(crate) async fn set_peer(&self, peer: Arc<dyn MediaPeer>) {
        if self.shared_media.is_some() {
            return; // Attached legs read from shared state
        }
        let mut runtime = self.runtime.write().await;
        runtime.peer = Some(peer);
    }

    pub(crate) async fn set_offer(&self, offer_sdp: Option<String>) {
        if self.shared_media.is_some() {
            return;
        }
        let mut runtime = self.runtime.write().await;
        runtime.offer_sdp = offer_sdp;
    }

    pub(crate) async fn set_answer(&self, answer_sdp: String) {
        if self.shared_media.is_some() {
            return;
        }
        let mut runtime = self.runtime.write().await;
        runtime.answer_sdp = Some(answer_sdp);
    }

    pub(crate) async fn offer_sdp(&self) -> Option<String> {
        if let Some(shared) = &self.shared_media {
            if let Ok(media) = shared.read() {
                return media.offer_sdp.clone();
            }
        }
        self.runtime.read().await.offer_sdp.clone()
    }

    pub(crate) async fn connected_dialog_id(&self) -> Option<String> {
        self.runtime.read().await.connected_dialog_id.clone()
    }

    pub(crate) async fn state(&self) -> RwiCallLegState {
        self.runtime.read().await.state
    }

    pub(crate) async fn set_negotiated_media(
        &self,
        negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
        ssrc: Option<u32>,
    ) {
        if self.shared_media.is_some() {
            return;
        }
        let mut runtime = self.runtime.write().await;
        runtime.negotiated_audio = negotiated_audio;
        runtime.ssrc = ssrc;
    }

    pub(crate) async fn set_connected_dialog_id(&self, dialog_id: String) {
        let mut runtime = self.runtime.write().await;
        runtime.connected_dialog_id = Some(dialog_id.clone());
        runtime.dialog_ids.insert(dialog_id);
    }

    pub(crate) async fn set_session_timer(&self, timer: Option<SessionTimerState>) {
        let mut runtime = self.runtime.write().await;
        runtime.session_timer = timer;
    }

    pub(crate) async fn session_timer(&self) -> Option<SessionTimerState> {
        self.runtime.read().await.session_timer.clone()
    }

    pub(crate) async fn try_begin_negotiation(&self, method: &str) -> Result<(), Option<String>> {
        let mut runtime = self.runtime.write().await;
        if runtime.negotiation_in_flight {
            return Err(runtime.pending_method.clone());
        }
        runtime.negotiation_in_flight = true;
        runtime.pending_method = Some(method.to_string());
        Ok(())
    }

    pub(crate) async fn finish_negotiation(&self) {
        let mut runtime = self.runtime.write().await;
        runtime.negotiation_in_flight = false;
        runtime.pending_method = None;
    }

    pub(crate) async fn is_negotiating(&self) -> bool {
        self.runtime.read().await.negotiation_in_flight
    }

    pub(crate) async fn pending_negotiation_method(&self) -> Option<String> {
        self.runtime.read().await.pending_method.clone()
    }

    pub(crate) async fn set_session_timer_refreshing(&self, refreshing: bool) {
        let mut runtime = self.runtime.write().await;
        if let Some(timer) = runtime.session_timer.as_mut() {
            timer.refreshing = refreshing;
        }
    }

    pub(crate) async fn update_session_timer_after_refresh(&self, success: bool) {
        let mut runtime = self.runtime.write().await;
        if let Some(timer) = runtime.session_timer.as_mut() {
            timer.refreshing = false;
            if success {
                timer.update_refresh();
            }
        }
    }

    pub(crate) async fn refresh_session_timer_from_headers(&self, headers: &rsip::Headers) {
        let mut runtime = self.runtime.write().await;
        let Some(timer) = runtime.session_timer.as_mut() else {
            return;
        };

        if let Some(value) = headers.iter().find_map(|header| match header {
            rsip::Header::Other(name, value) if name.eq_ignore_ascii_case(HEADER_SESSION_EXPIRES) => {
                Some(value.clone())
            }
            _ => None,
        }) {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                timer.enabled = true;
                timer.active = true;
                timer.session_interval = interval;
                timer.refresher = refresher.unwrap_or(SessionRefresher::Uac);
                timer.update_refresh();
                return;
            }
        }

        if timer.active {
            timer.update_refresh();
        }
    }

    pub(crate) async fn mark_bridged(&self) {
        let mut runtime = self.runtime.write().await;
        runtime.state = RwiCallLegState::Bridged;
    }

    pub(crate) async fn live_media(&self) -> Option<RwiLiveLegMedia> {
        if let Some(shared) = &self.shared_media {
            let media = shared.read().ok()?;
            return Some(RwiLiveLegMedia {
                peer: media.peer.clone()?,
                negotiated_audio: media.negotiated_audio.clone()?,
                ssrc: media.ssrc,
            });
        }
        let runtime = self.runtime.read().await;
        Some(RwiLiveLegMedia {
            peer: runtime.peer.clone()?,
            negotiated_audio: runtime.negotiated_audio.clone()?,
            ssrc: runtime.ssrc,
        })
    }

    pub(crate) async fn clear_runtime(&self) {
        let mut runtime = self.runtime.write().await;
        runtime.peer = None;
        runtime.negotiated_audio = None;
        runtime.ssrc = None;
        runtime.connected_dialog_id = None;
        runtime.dialog_ids.clear();
        runtime.state = RwiCallLegState::Terminated;
        runtime.session_timer = None;
        runtime.negotiation_in_flight = false;
        runtime.pending_method = None;
    }

    pub(crate) async fn info(&self) -> RwiCallLegInfo {
        self.info.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaStreamBuilder;
    use crate::proxy::proxy_call::media_peer::VoiceEnginePeer;
    use crate::proxy::proxy_call::session_timer::{SessionRefresher, SessionTimerState};

    #[tokio::test]
    async fn test_outbound_leg_command_handle_sends_hangup() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("rwi-leg-test".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "call-1".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("caller".to_string()),
            Some("callee".to_string()),
        );

        leg.command_handle()
            .send_action(SessionAction::Hangup {
                reason: None,
                code: Some(16),
                initiator: Some("test".to_string()),
            })
            .expect("standalone leg should accept hangup");

        let cmd = rx.recv().await.expect("hangup command should be queued");
        assert!(matches!(
            cmd,
            LegCommand::Hangup {
                reason: None,
                code: Some(16),
                ref initiator,
            } if initiator.as_deref() == Some("test")
        ));
    }

    #[tokio::test]
    async fn test_outbound_leg_command_handle_sends_hold_and_unhold() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("rwi-leg-hold-test".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "call-hold".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("caller".to_string()),
            Some("callee".to_string()),
        );

        leg.command_handle()
            .send_action(SessionAction::Hold {
                music_source: Some("hold.wav".to_string()),
            })
            .expect("standalone leg should accept hold");
        let cmd = rx.recv().await.expect("hold command should be queued");
        assert!(matches!(
            cmd,
            LegCommand::Hold { ref music_source } if music_source.as_deref() == Some("hold.wav")
        ));

        leg.command_handle()
            .send_action(SessionAction::Unhold)
            .expect("standalone leg should accept unhold");
        let cmd = rx.recv().await.expect("unhold command should be queued");
        assert!(matches!(cmd, LegCommand::Unhold));
    }

    #[tokio::test]
    async fn test_outbound_leg_command_handle_sends_playback_commands() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("rwi-leg-playback-test".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "call-play".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("caller".to_string()),
            Some("callee".to_string()),
        );

        leg.command_handle()
            .send_action(SessionAction::PlayPrompt {
                audio_file: "welcome.wav".to_string(),
                send_progress: false,
                await_completion: false,
                track_id: Some("track-1".to_string()),
                loop_playback: true,
                interrupt_on_dtmf: true,
            })
            .expect("standalone leg should accept media play");
        let cmd = rx.recv().await.expect("play command should be queued");
        assert!(matches!(
            cmd,
            LegCommand::PlayAudio { ref file, loop_playback: true, .. } if file == "welcome.wav"
        ));

        leg.command_handle()
            .send_action(SessionAction::StopPlayback)
            .expect("standalone leg should accept media stop");
        let cmd = rx.recv().await.expect("stop command should be queued");
        assert!(matches!(cmd, LegCommand::StopPlayback));
    }

    #[tokio::test]
    async fn test_outbound_leg_refreshes_session_timer_from_headers() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("rwi-leg-timer-test".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "call-timer".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("caller".to_string()),
            Some("callee".to_string()),
        );
        leg.set_session_timer(Some(SessionTimerState::default())).await;

        let response = rsip::Response::try_from(
            "SIP/2.0 200 OK\r\nSession-Expires: 120;refresher=uas\r\n\r\n",
        )
        .expect("response should parse");
        leg.refresh_session_timer_from_headers(&response.headers).await;

        let timer = leg.session_timer().await.expect("timer should exist");
        assert!(timer.active);
        assert_eq!(timer.session_interval.as_secs(), 120);
        assert_eq!(timer.refresher, SessionRefresher::Uas);
    }

    #[tokio::test]
    async fn test_outbound_leg_negotiation_guard_tracks_pending_method() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let peer = Arc::new(VoiceEnginePeer::new(Arc::new(
            MediaStreamBuilder::new()
                .with_id("rwi-leg-negotiation-test".to_string())
                .build(),
        )));
        let leg = RwiCallLeg::new_outbound(
            "call-negotiation".to_string(),
            tx,
            peer,
            Some("offer".to_string()),
            CancellationToken::new(),
            Some("caller".to_string()),
            Some("callee".to_string()),
        );

        leg.try_begin_negotiation("INVITE")
            .await
            .expect("first negotiation should start");
        assert!(leg.is_negotiating().await);
        assert_eq!(
            leg.pending_negotiation_method().await.as_deref(),
            Some("INVITE")
        );
        assert_eq!(
            leg.try_begin_negotiation("UPDATE").await.err(),
            Some(Some("INVITE".to_string()))
        );

        leg.finish_negotiation().await;
        assert!(!leg.is_negotiating().await);
        assert_eq!(leg.pending_negotiation_method().await, None);
    }
}
