use crate::call::sip::{DialogStateReceiverGuard, ServerDialogGuard};
use crate::media::mixer::SupervisorMixerMode;
use crate::media::negotiate::MediaNegotiator;
use crate::media::negotiate::CodecInfo;
use crate::proxy::proxy_call::answer_runtime::AnswerRuntime;
use crate::proxy::proxy_call::app_runtime::AppRuntime;
use crate::proxy::proxy_call::bridge_runtime::BridgeRuntime;
use crate::proxy::proxy_call::call_leg::{CallLeg, CallLegDirection};
use crate::proxy::proxy_call::dialplan_runtime::DialplanRuntime;
use crate::proxy::proxy_call::originated_runtime::OriginatedRuntime;
use crate::proxy::proxy_call::media_endpoint::{BridgeSelection, MediaEndpoint};
use crate::proxy::proxy_call::queue_flow::QueueFlow;
use crate::proxy::proxy_call::recording_runtime::RecordingState;
use crate::proxy::proxy_call::reporter::CallReporter;
use crate::proxy::proxy_call::session_loop_runtime::SessionLoopRuntime;
use crate::proxy::proxy_call::sip_leg::SipLeg;
use crate::proxy::proxy_call::target_runtime::TargetRuntime;
use crate::proxy::routing::matcher::RouteResourceLookup;
use crate::{
    call::{
        CallForwardingConfig, CallForwardingMode, DialStrategy, DialplanFlow, Location,
        TransferEndpoint,
    },
    callrecord::{CallRecordHangupMessage, CallRecordHangupReason, CallRecordSender},
    config::{MediaProxyMode, RouteResult},
    proxy::{
        proxy_call::{
            media_peer::{MediaPeer, VoiceEnginePeer},
            state::{
                CallContext, CallSessionHandle, CallSessionShared, MidDialogLeg,
                ProxyCallEvent, SessionAction,
            },
        },
        server::SipServerRef,
    },
};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use futures::{FutureExt, future::BoxFuture};
use rsip::StatusCode;
use rsip::Uri;
use rsipstack::dialog::{
    DialogId, dialog::DialogState, dialog_layer::DialogLayer, invitation::InviteOption,
    server_dialog::ServerInviteDialog,
};
use rsipstack::transaction::key::TransactionRole;
use rustrtc;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug)]
pub(crate) enum FlowFailureHandling {
    Handle,
    #[allow(dead_code)]
    Propagate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationState {
    Idle,
    Stable,
    LocalOfferSent,
    RemoteOfferReceived,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingHangup {
    pub reason: Option<CallRecordHangupReason>,
    pub code: Option<u16>,
    pub initiator: Option<String>,
}

/// Parameters for an RWI-originated outbound dial, stored on CallSession
/// so that `process()` can dispatch the originated flow instead of dialplan.
pub(crate) struct OriginatedDialParams {
    pub invite_option: InviteOption,
    pub timeout_secs: u64,
    /// Channel for emitting RWI events back to the processor.
    pub event_tx: Option<mpsc::UnboundedSender<OriginatedSessionEvent>>,
}

/// Events emitted by an originated session back to the RWI layer.
#[derive(Debug)]
pub enum OriginatedSessionEvent {
    Ringing,
    EarlyMedia,
    Answered,
    Failed { reason: String, sip_status: Option<u16> },
    Hangup { reason: String },
    Busy,
}

/// Tracks the termination lifecycle so the session waits for actual SIP
/// cleanup completion (or a grace timeout) before finalizing.
pub(crate) struct TerminationState {
    pub started_at: Instant,
    pub caller_cleanup_sent: bool,
    pub callee_cleanup_sent: bool,
    pub grace_deadline: Instant,
}

impl TerminationState {
    pub fn new(grace_secs: u64) -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            caller_cleanup_sent: false,
            callee_cleanup_sent: false,
            grace_deadline: now + Duration::from_secs(grace_secs),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SessionHangupMessage {
    pub code: u16,
    pub reason: Option<String>,
    pub target: Option<String>,
}

impl From<&SessionHangupMessage> for CallRecordHangupMessage {
    fn from(message: &SessionHangupMessage) -> Self {
        Self {
            code: message.code,
            reason: message.reason.clone(),
            target: message.target.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ParallelEvent {
    Calling {
        _idx: usize,
        dialog_id: DialogId,
    },
    Early {
        _idx: usize,
        dialog_id: DialogId,
        sdp: Option<String>,
    },
    Accepted {
        _idx: usize,
        dialog_id: DialogId,
        answer: String,
        aor: String,
        caller_uri: String,
        callee_uri: String,
        contact: String,
        destination: Option<String>,
    },
    Failed {
        #[allow(dead_code)]
        _idx: usize,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    },
    Terminated {
        _idx: usize,
    },
    Cancelled,
}

#[derive(Debug)]
pub(crate) struct SessionActionInbox {
    rx: mpsc::UnboundedReceiver<SessionAction>,
}

impl SessionActionInbox {
    pub fn new(rx: mpsc::UnboundedReceiver<SessionAction>) -> Self {
        Self { rx }
    }

    pub fn try_recv(&mut self) -> Result<SessionAction, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    pub async fn recv(&mut self) -> Option<SessionAction> {
        self.rx.recv().await
    }
}

pub(crate) type ActionInbox<'a> = Option<&'a mut SessionActionInbox>;

pub(crate) struct CallSessionRecordSnapshot {
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<CallRecordHangupMessage>,
    pub original_caller: Option<String>,
    pub original_callee: Option<String>,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub connected_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub last_queue_name: Option<String>,
    pub target_dialogs: Vec<DialogId>,
    pub server_dialog_id: Option<DialogId>,
    pub extensions: http::Extensions,
}

pub(crate) struct CallSession {
    pub server: SipServerRef,
    pub dialog_layer: Arc<DialogLayer>,
    pub cancel_token: CancellationToken,
    pub call_record_sender: Option<CallRecordSender>,
    pub pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
    pub context: CallContext,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub connected_callee: Option<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub shared: CallSessionShared,
    pub exported_leg: CallLeg,
    pub target_leg: Option<CallLeg>,
    pub bridge_runtime: BridgeRuntime,
    pub use_media_proxy: bool,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    pub handle: Option<CallSessionHandle>,
    /// Channel used to deliver [`ControllerEvent`]s to the running `CallApp` event loop.
    /// Populated by [`run_application`] for the lifetime of the app; cleared when the
    /// app exits so stale senders are never accidentally reused.
    pub app_event_tx: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    /// Current recording state (path, start time).
    pub recording_state: RecordingState,
    /// Termination tracking: present once `enter_terminating()` has been called.
    pub termination_state: Option<TerminationState>,
    /// Parameters for originated dial flow (only set for RwiSingleLeg sessions).
    pub originated_dial_params: Option<OriginatedDialParams>,
    /// Sender for target dialog events, held until target leg is created during dial.
    pub target_dialog_event_tx: Option<mpsc::UnboundedSender<rsipstack::dialog::dialog::DialogState>>,
}

impl CallSession {
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    pub const RINGBACK_TRACK_ID: &'static str = "ringback-track";

    fn check_media_proxy(
        context: &CallContext,
        offer_sdp: &str,
        mode: &MediaProxyMode,
        all_webrtc_target: bool,
    ) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }
        match mode {
            MediaProxyMode::All => true,
            MediaProxyMode::None => false,
            MediaProxyMode::Nat => false, // TODO: Implement NAT detection
            MediaProxyMode::Auto => {
                // If caller is WebRTC but not all targets are WebRTC, we need media proxy to transcode and bridge
                // If caller is not WebRTC but all targets are WebRTC, we also need media proxy to transcode and bridge
                let caller_is_webrtc = Self::is_webrtc_sdp(offer_sdp);
                if caller_is_webrtc && !all_webrtc_target {
                    return true;
                }
                if !caller_is_webrtc && all_webrtc_target {
                    return true;
                }
                false
            }
        }
    }

    pub fn new(
        server: SipServerRef,
        dialog_layer: Arc<DialogLayer>,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        server_dialog: Option<ServerInviteDialog>,
        use_media_proxy: bool,
        recorder_option: Option<crate::media::recorder::RecorderOption>,
        exported_peer: Arc<dyn MediaPeer>,
        target_peer: Option<Arc<dyn MediaPeer>>,
        shared: CallSessionShared,
        reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    ) -> Self {
        let caller_offer = server_dialog.as_ref().and_then(|sd| {
            let initial = sd.initial_request();
            if initial.body().is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(initial.body()).to_string())
            }
        });

        let leg_id = context.session_id.clone();
        let mut exported_leg = CallLeg::new(
            leg_id.clone(),
            crate::proxy::proxy_call::call_leg::LegRole::Caller,
            CallLegDirection::Inbound,
            exported_peer,
            caller_offer,
        );
        if let Some(sd) = server_dialog {
            exported_leg.set_server_dialog(sd);
        }

        let session = Self {
            server,
            dialog_layer,
            cancel_token,
            call_record_sender,
            pending_hangup: Arc::new(Mutex::new(None)),
            context,
            last_error: None,
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            shared,
            exported_leg,
            target_leg: target_peer.map(|peer| CallLeg::new(
                leg_id,
                crate::proxy::proxy_call::call_leg::LegRole::Callee,
                CallLegDirection::Outbound,
                peer,
                None,
            )),
            bridge_runtime: BridgeRuntime::new(recorder_option),
            use_media_proxy,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            reporter,
            handle: None,
            app_event_tx: None,
            recording_state: None,
            termination_state: None,
            originated_dial_params: None,
            target_dialog_event_tx: None,
        };
        session
    }

    pub(crate) fn target_leg(&self) -> &CallLeg {
        self.target_leg.as_ref().expect("target_leg not yet created")
    }

    pub(crate) fn target_leg_mut(&mut self) -> &mut CallLeg {
        self.target_leg.as_mut().expect("target_leg not yet created")
    }

    /// The session's externally-visible leg for control surfaces (RWI, etc.).
    pub(crate) fn exported_leg(&self) -> &CallLeg {
        &self.exported_leg
    }

    pub(crate) fn exported_leg_mut(&mut self) -> &mut CallLeg {
        &mut self.exported_leg
    }

    /// Ensures the target leg exists, creating it on demand if absent.
    /// Used by both proxy and originated paths to defer target creation until dialing.
    pub(crate) fn ensure_target_leg(&mut self) {
        if self.target_leg.is_some() {
            return;
        }
        let target_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", self.context.session_id))
            .with_cancel_token(self.cancel_token.child_token());
        let target_peer: Arc<dyn MediaPeer> = Arc::new(VoiceEnginePeer::new(Arc::new(target_media_builder.build())));
        self.target_leg = Some(CallLeg::new(
            self.context.session_id.clone(),
            crate::proxy::proxy_call::call_leg::LegRole::Callee,
            CallLegDirection::Outbound,
            target_peer,
            None,
        ));
        // Set dialog event channel if one was stored
        if let Some(tx) = self.target_dialog_event_tx.take() {
            self.target_leg_mut().sip.dialog_event_tx = Some(tx);
        }
    }

    fn explicit_audio_default_selection() -> BridgeSelection {
        BridgeSelection {
            codec: CodecType::PCMU,
            params: rustrtc::RtpCodecParameters {
                payload_type: CodecType::PCMU.payload_type(),
                clock_rate: CodecType::PCMU.clock_rate(),
                channels: CodecType::PCMU.channels() as u8,
            },
            dtmf_codecs: Vec::new(),
            ssrc: None,
        }
    }

    pub(crate) fn get_retry_codes(&self) -> Option<&Vec<u16>> {
        match &self.context.dialplan.flow {
            crate::call::DialplanFlow::Queue { plan, .. } => plan.retry_codes.as_ref(),
            _ => None,
        }
    }

    pub fn add_callee_guard(&mut self, mut guard: DialogStateReceiverGuard) {
        if let Some(tx) = &self.target_leg().sip.dialog_event_tx {
            if let Some(mut receiver) = guard.take_receiver() {
                let tx = tx.clone();
                let cancel_token = self.cancel_token.clone();
                crate::utils::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = cancel_token.cancelled() => break,
                            state = receiver.recv() => {
                                if let Some(state) = state {
                                    let _ = tx.send(state);
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
        self.target_leg_mut().sip.dialog_guards.push(guard);
    }

    pub fn init_server_timer(
        &mut self,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let initial_request = self.exported_leg.server_initial_request()
            .ok_or((StatusCode::ServerInternalError, Some("No server dialog".to_string())))?;
        self.exported_leg
            .sip
            .init_timer_from_initial_request(&initial_request, default_expires)
    }

    pub fn init_client_timer(&mut self, response: &rsip::Response, default_expires: u64) {
        self.target_leg_mut()
            .sip
            .init_timer_from_final_response(response, default_expires);
    }

    pub fn note_attempt_failure(
        &mut self,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    ) {
        self.hangup_messages.push(SessionHangupMessage {
            code: u16::from(code.clone()),
            reason: reason.clone(),
            target: target.clone(),
        });
        self.shared.emit_custom_event(ProxyCallEvent::TargetFailed {
            session_id: self.shared.session_id(),
            target,
            code: Some(u16::from(code)),
            reason,
        });
    }

    fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }

    pub fn register_active_call(&mut self, handle: CallSessionHandle) {
        self.shared.register_active_call(handle.clone());
        if let Some(dialog_id) = self.exported_leg.server_dialog_id() {
            self.shared
                .register_dialog(dialog_id.to_string(), handle.clone());
        }
        self.handle = Some(handle);

        // Publish exported leg media to shared state so RWI consumers can read it
        self.publish_exported_leg_media();
    }

    /// Publish exported leg media state to the shared struct for RWI consumption.
    pub(crate) fn publish_exported_leg_media(&self) {
        let leg = self.exported_leg();
        self.shared.publish_exported_leg_media(
            leg.media.peer.clone(),
            leg.media.offer_sdp.clone(),
            leg.media.answer_sdp.clone(),
            leg.media.negotiated_audio.clone(),
            leg.media.answered_ssrc(),
            self.bridge_runtime.is_active(),
        );
    }

    pub fn last_queue_name(&self) -> Option<String> {
        None
        // self.last_queue_name.clone()
    }

    pub fn record_snapshot(&mut self) -> CallSessionRecordSnapshot {
        CallSessionRecordSnapshot {
            ring_time: self.ring_time,
            answer_time: self.answer_time,
            last_error: self.last_error.clone(),
            hangup_reason: self.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: self.routed_caller.clone(),
            routed_callee: self.routed_callee.clone(),
            connected_callee: self.connected_callee.clone(),
            routed_contact: self.routed_contact.clone(),
            routed_destination: self.routed_destination.clone(),
            last_queue_name: self.last_queue_name(),
            target_dialogs: self.target_leg.as_ref().map(|l| l.sip.recorded_dialogs()).unwrap_or_default(),
            server_dialog_id: self.exported_leg.server_dialog_id(),
            extensions: self.context.dialplan.extensions.clone(),
        }
    }

    pub fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
        self.refresh_active_call_parties();
        let current_target = self
            .routed_callee
            .clone()
            .or_else(|| Some(self.context.original_callee.clone()));
        self.shared.set_current_target(current_target);
    }

    fn refresh_active_call_parties(&self) {
        self.shared
            .update_routed_parties(self.routed_caller.clone(), self.routed_callee.clone());
    }

    pub(crate) fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("RTP/SAVPF")
    }

    pub(crate) async fn build_caller_answer(&mut self, codec_info: Vec<CodecInfo>) -> Result<String> {
        let caller_offer_sdp = self.exported_leg.media.offer_sdp.clone();
        self.exported_leg
            .media
            .create_caller_answer(
                "caller-track",
                codec_info,
                caller_offer_sdp.as_deref(),
                &self.context.media_config,
            )
            .await
    }

    pub(crate) async fn build_caller_answer_trickle(
        &mut self,
        codec_info: Vec<CodecInfo>,
    ) -> Result<String> {
        let caller_offer_sdp = self.exported_leg.media.offer_sdp.clone();
        self.exported_leg
            .media
            .create_caller_answer_trickle(
                "caller-track",
                codec_info,
                caller_offer_sdp.as_deref(),
                &self.context.media_config,
            )
            .await
    }

    async fn ensure_media_bridge_for_selection(
        &mut self,
        caller_selection: BridgeSelection,
        callee_selection: BridgeSelection,
    ) {
        self.bridge_runtime.ensure_media_bridge(
            self.exported_leg.media.peer.clone(),
            self.target_leg().media.peer.clone(),
            caller_selection.params.clone(),
            callee_selection.params.clone(),
            caller_selection.dtmf_codecs.clone(),
            callee_selection.dtmf_codecs.clone(),
            caller_selection.codec,
            callee_selection.codec,
            caller_selection.ssrc,
            callee_selection.ssrc,
            self.context
                .dialplan
                .call_id
                .clone()
                .unwrap_or_else(|| self.context.session_id.clone()),
            self.server.sip_flow.as_ref().and_then(|sf| sf.backend()),
        );
    }

    pub(crate) async fn ensure_media_bridge_from_sdp(
        &mut self,
        callee_sdp: &str,
        match_caller_to_callee_codec: bool,
        log_label: &str,
    ) {
        if self.bridge_runtime.media_bridge.is_some() {
            return;
        }

        let default_codec = Self::explicit_audio_default_selection();
        let callee_selection = MediaEndpoint::bridge_selection_from_sdp(
            callee_sdp,
            &self.context.dialplan.allow_codecs,
        )
        .or_else(|| {
            self.exported_leg
                .media.bridge_audio_selection(&self.context.dialplan.allow_codecs)
        })
        .unwrap_or(default_codec.clone());

        let caller_selection = if match_caller_to_callee_codec {
            self.exported_leg
                .media.bridge_audio_matching(Some(callee_selection.codec))
                .unwrap_or(default_codec)
        } else {
            self.exported_leg
                .media.bridge_audio_selection(&self.context.dialplan.allow_codecs)
                .unwrap_or(default_codec)
        };

        debug!(
            session_id = %self.context.session_id,
            codec_a = ?caller_selection.codec,
            params_a = ?caller_selection.params,
            codec_b = ?callee_selection.codec,
            params_b = ?callee_selection.params,
            ssrc_a = caller_selection.ssrc,
            ssrc_b = callee_selection.ssrc,
            bridge_path = log_label,
            "Preparing media bridge for call session"
        );

        self.ensure_media_bridge_for_selection(caller_selection, callee_selection)
            .await;
    }


    /// Create offer SDP for a specific target based on its WebRTC support
    pub(crate) async fn create_offer_for_target(&mut self, target: &Location) -> Option<Vec<u8>> {
        info!(
            session_id = %self.context.session_id,
            target = %target.aor,
            supports_webrtc = target.supports_webrtc,
            destination = ?target.destination,
            "create_offer_for_target called"
        );

        // Ensure callee leg exists for outbound dialing
        self.ensure_target_leg();

        if !self.use_media_proxy {
            // Media proxy disabled: use caller's original offer
            return match self.exported_leg.media.offer_sdp.as_ref() {
                Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                _ => None,
            };
        }

        // Media proxy enabled: generate SDP based on target's WebRTC support
        match self.create_target_track(target.supports_webrtc).await {
            Ok(sdp) if !sdp.trim().is_empty() => Some(sdp.into_bytes()),
            Ok(_) => {
                warn!(
                    session_id = %self.context.session_id,
                    target = %target.aor,
                    supports_webrtc = target.supports_webrtc,
                    "Generated empty SDP for target"
                );
                None
            }
            Err(e) => {
                warn!(
                    session_id = %self.context.session_id,
                    target = %target.aor,
                    supports_webrtc = target.supports_webrtc,
                    error = %e,
                    "Failed to create callee track for target"
                );
                // Fallback to using the default callee offer
                match self.target_leg().media.offer_sdp.as_ref() {
                    Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                    _ => None,
                }
            }
        }
    }

    pub async fn create_target_track(&mut self, is_webrtc: bool) -> Result<String> {
        let caller_offer = self.exported_leg.media.offer_sdp.clone();
        let callee = self.target_leg.as_mut().expect("target_leg not yet created");
        callee
            .media
            .create_local_track_offer(
                Self::CALLEE_TRACK_ID.to_string(),
                is_webrtc,
                caller_offer.as_deref(),
                &self.context.dialplan.allow_codecs,
                &self.context.media_config,
            )
            .await
    }

    pub(crate) async fn setup_target_track(
        &mut self,
        callee_answer_sdp: &String,
        _dialog_id: Option<&DialogId>,
    ) -> Result<()> {
        {
            let callee = self.target_leg.as_mut().expect("target_leg not yet created");
            callee
                .media
                .apply_remote_answer(
                    Self::CALLEE_TRACK_ID,
                    callee_answer_sdp,
                    &self.context.media_config,
                )
                .await?;
        }
        let negotiated = MediaEndpoint::select_best_audio_from_sdp(
            callee_answer_sdp,
            &self.context.dialplan.allow_codecs,
        );
        self.target_leg_mut().media.select_or_store_negotiated_audio(negotiated);
        Ok(())
    }

    pub fn add_callee_dialog(&mut self, dialog_id: DialogId) {
        {
            let target_dialogs = self.target_leg().sip.active_dialog_ids.lock().unwrap();
            if target_dialogs.contains(&dialog_id) {
                return;
            }
        }
        self.target_leg_mut().sip.add_dialog(dialog_id.clone());
        if let Some(handle) = &self.handle {
            self.shared
                .register_dialog(dialog_id.to_string(), handle.clone());
        }
    }

    pub async fn start_ringing(&mut self, answer: String) {
        AnswerRuntime::start_ringing(self, answer).await;
    }

    fn media_direction_from_sdp(sdp: &str) -> &'static str {
        if sdp.contains("\r\na=inactive\r\n") || sdp.ends_with("\r\na=inactive") {
            "inactive"
        } else if sdp.contains("\r\na=sendonly\r\n") || sdp.ends_with("\r\na=sendonly") {
            "sendonly"
        } else if sdp.contains("\r\na=recvonly\r\n") || sdp.ends_with("\r\na=recvonly") {
            "recvonly"
        } else {
            "sendrecv"
        }
    }

    fn rewrite_sdp_direction(answer_sdp: &str, offer_sdp: &str) -> String {
        let answer_direction = match Self::media_direction_from_sdp(offer_sdp) {
            "sendonly" => "recvonly",
            "recvonly" => "sendonly",
            "inactive" => "inactive",
            _ => "sendrecv",
        };

        let lines: Vec<String> = answer_sdp
            .split("\r\n")
            .filter(|line| !line.is_empty())
            .filter(|line| {
                !matches!(
                    *line,
                    "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
                )
            })
            .map(str::to_string)
            .collect();

        let mut rewritten = Vec::with_capacity(lines.len() + 2);
        let mut inserted = false;
        for line in lines {
            rewritten.push(line.clone());
            if !inserted && line.starts_with("m=audio ") {
                rewritten.push(format!("a={answer_direction}"));
                inserted = true;
            }
        }
        if !inserted {
            rewritten.push(format!("a={answer_direction}"));
        }
        rewritten.join("\r\n") + "\r\n"
    }

    fn current_callee_codec(&self) -> Option<CodecType> {
        self.target_leg()
            .media
            .negotiated_audio
            .as_ref()
            .map(|(codec, _, _)| *codec)
            .or_else(|| {
                self.target_leg()
                    .media
                    .answer_sdp
                    .as_deref()
                    .and_then(|sdp| {
                        MediaEndpoint::select_best_audio_from_sdp(
                            sdp,
                            &self.context.dialplan.allow_codecs,
                        )
                    })
                    .map(|(codec, _, _)| codec)
            })
            .or_else(|| {
                self.target_leg()
                    .media
                    .offer_sdp
                    .as_deref()
                    .and_then(|sdp| {
                        MediaEndpoint::select_best_audio_from_sdp(
                            sdp,
                            &self.context.dialplan.allow_codecs,
                        )
                    })
                    .map(|(codec, _, _)| codec)
            })
    }

    fn offer_supports_current_callee_codec(&self, offer_sdp: &str) -> bool {
        let Some(current_codec) = self.current_callee_codec() else {
            return false;
        };
        MediaNegotiator::extract_codec_params(offer_sdp)
            .audio
            .iter()
            .any(|codec| codec.codec == current_codec)
    }

    async fn respond_to_mid_dialog(
        &self,
        leg: MidDialogLeg,
        dialog_id: &str,
        status: StatusCode,
        body: Option<String>,
    ) -> bool {
        let Some(handle) = self.shared.take_mid_dialog_reply(leg.clone(), dialog_id) else {
            warn!(
                session_id = %self.context.session_id,
                ?leg,
                dialog_id,
                %status,
                "Missing pending mid-dialog reply handle"
            );
            return false;
        };

        let result = if let Some(body) = body {
            let headers = Some(vec![rsip::Header::ContentType("application/sdp".into())]);
            handle.respond(status, headers, Some(body.into_bytes())).await
        } else {
            handle.reply(status).await
        };

        if let Err(err) = result {
            warn!(
                session_id = %self.context.session_id,
                ?leg,
                dialog_id,
                error = %err,
                "Failed to send mid-dialog response"
            );
            return false;
        }

        true
    }

    async fn handle_callee_reinvite(
        &mut self,
        method: rsip::Method,
        dialog_id: String,
        sdp: Option<String>,
    ) {
        info!(
            session_id = %self.context.session_id,
            ?method,
            dialog_id,
            "Handling mid-dialog request on callee leg"
        );

        let response_body = if let Some(offer) = sdp {
            self.target_leg_mut().negotiation_state = NegotiationState::RemoteOfferReceived;

            if !self.offer_supports_current_callee_codec(&offer) {
                warn!(
                    session_id = %self.context.session_id,
                    ?method,
                    "Rejecting callee re-INVITE because current negotiated codec is not offered"
                );
                self.respond_to_mid_dialog(
                    MidDialogLeg::Callee,
                    &dialog_id,
                    StatusCode::NotAcceptableHere,
                    None,
                )
                .await;
                self.target_leg_mut().negotiation_state = NegotiationState::Stable;
                return;
            }

            let base_answer = self
                .target_leg()
                .media
                .answer_sdp
                .clone()
                .or_else(|| self.target_leg().media.offer_sdp.clone());
            let Some(base_answer) = base_answer else {
                warn!(
                    session_id = %self.context.session_id,
                    ?method,
                    "Missing existing negotiated SDP for callee re-INVITE"
                );
                self.respond_to_mid_dialog(
                    MidDialogLeg::Callee,
                    &dialog_id,
                    StatusCode::NotAcceptableHere,
                    None,
                )
                .await;
                self.target_leg_mut().negotiation_state = NegotiationState::Stable;
                return;
            };

            let offered_direction = Self::media_direction_from_sdp(&offer);
            let answer = Self::rewrite_sdp_direction(&base_answer, &offer);

            self.target_leg_mut().media.offer_sdp = Some(offer);
            self.target_leg_mut().media.answer_sdp = Some(answer.clone());

            let bridge_result = match offered_direction {
                "sendonly" | "inactive" => {
                    self.bridge_runtime
                        .suppress_or_pause_callee_forwarding(
                            Self::CALLEE_TRACK_ID,
                            self.exported_leg.media.peer.clone(),
                        )
                        .await
                }
                _ => {
                    self.bridge_runtime
                        .resume_or_unpause_callee_forwarding(
                            Self::CALLEE_TRACK_ID,
                            self.exported_leg.media.peer.clone(),
                        )
                        .await
                }
            };
            if let Err(err) = bridge_result {
                warn!(
                    session_id = %self.context.session_id,
                    direction = offered_direction,
                    error = %err,
                    "Failed to update bridge forwarding for callee re-INVITE"
                );
            }

            self.target_leg_mut().negotiation_state = NegotiationState::Stable;
            Some(answer)
        } else {
            self.target_leg_mut().negotiation_state = NegotiationState::Stable;
            if method == rsip::Method::Update {
                None
            } else {
                self.target_leg().media.answer_sdp.clone()
            }
        };

        self.respond_to_mid_dialog(MidDialogLeg::Callee, &dialog_id, StatusCode::OK, response_body)
            .await;
    }

    pub async fn handle_reinvite(
        &mut self,
        leg: MidDialogLeg,
        method: rsip::Method,
        dialog_id: String,
        sdp: Option<String>,
    ) {
        if self.exported_leg.media.answer_sdp.is_none() {
            self.exported_leg.media.answer_sdp = self.shared.answer_sdp();
        }
        info!(
            session_id = %self.context.session_id,
            ?leg,
            ?method,
            dialog_id,
            has_answer = self.exported_leg.media.answer_sdp.is_some(),
            "Handle re-INVITE/UPDATE"
        );

        if leg == MidDialogLeg::Callee {
            self.handle_callee_reinvite(method, dialog_id, sdp).await;
            return;
        }

        if let Some(offer) = sdp {
            debug!(?method, "Received Re-invite/UPDATE with SDP (Offer)");
            self.exported_leg.negotiation_state = NegotiationState::RemoteOfferReceived;

            if let Err(e) = self.exported_leg.media.update_remote_offer("caller-track", &offer).await {
                warn!(?method, "Failed to update caller peer with offer: {}", e);
                self.respond_to_mid_dialog(
                    MidDialogLeg::Caller,
                    &dialog_id,
                    StatusCode::NotAcceptableHere,
                    None,
                )
                .await;
                self.exported_leg.negotiation_state = NegotiationState::Stable;
                return;
            }

            self.target_leg()
                .sip
                .forward_remote_offer_to_client_dialog(
                    self.dialog_layer.clone(),
                    method.clone(),
                    offer.clone(),
                )
                .await;

            let body = self.exported_leg.media.answer_sdp.clone();
            let status = if body.is_none() {
                StatusCode::NotAcceptableHere
            } else {
                StatusCode::OK
            };
            self.respond_to_mid_dialog(MidDialogLeg::Caller, &dialog_id, status, body)
                .await;
            self.exported_leg.negotiation_state = NegotiationState::Stable;
        } else {
            debug!(
                ?method,
                "Received Re-invite/UPDATE without SDP (Request for Offer)"
            );
            self.exported_leg.negotiation_state = NegotiationState::LocalOfferSent;
            let body = if method == rsip::Method::Invite {
                self.exported_leg.media.answer_sdp.clone()
            } else {
                None
            };
            self.respond_to_mid_dialog(MidDialogLeg::Caller, &dialog_id, StatusCode::OK, body)
                .await;
            self.exported_leg.negotiation_state = NegotiationState::Stable;
        }
    }

    pub async fn handle_trickle_ice(&mut self, payload: &str) {
        for line in payload.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let candidate = line
                .strip_prefix("a=candidate:")
                .or_else(|| line.strip_prefix("candidate:"));
            if let Some(candidate) = candidate {
                if let Err(err) = self
                    .exported_leg
                    .media
                    .add_remote_ice_candidate("caller-track", candidate.trim())
                    .await
                {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %err,
                        "Failed to add remote trickle ICE candidate"
                    );
                }
            }
        }
    }

    pub fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        debug!(code = %code, reason = ?reason, target = ?target, "Call session error set");
        self.last_error = Some((code.clone(), reason.clone()));
        self.hangup_reason = Some(CallRecordHangupReason::Failed);
        self.note_attempt_failure(code.clone(), reason.clone(), target);
        self.shared.note_failure(code, reason);
    }

    pub fn is_answered(&self) -> bool {
        self.answer_time.is_some()
    }

    pub fn set_answer(&mut self, sdp: String) {
        self.shared.set_answer_sdp(sdp.clone());
        self.exported_leg.media.answer_sdp = Some(sdp);
    }

    pub(crate) fn freeze_answered_caller_audio(&mut self) {
        self.exported_leg
            .media
            .freeze_answered_audio_with_allow_codecs(&self.context.dialplan.allow_codecs);
    }

    pub async fn accept_call(
        &mut self,
        callee: Option<String>,
        callee_answer: Option<String>,
        dialog_id: Option<String>,
    ) -> Result<()> {
        AnswerRuntime::accept_call(self, callee, callee_answer, dialog_id).await
    }

    pub(crate) fn mark_active_call_answered(&self) {
        self.shared.transition_to_answered();
    }

    pub fn report_failure(&self, code: StatusCode, reason: Option<String>) -> Result<()> {
        let reporter = CallReporter {
            server: self.server.clone(),
            context: self.context.clone(),
            call_record_sender: self.call_record_sender.clone(),
        };

        let server_dialog_id = DialogId::try_from((
            self.context.dialplan.original.as_ref(),
            TransactionRole::Server,
        ))?;

        let snapshot = CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: Some((code.clone(), reason.clone())),
            hangup_reason: Some(CallRecordHangupReason::Failed),
            hangup_messages: vec![CallRecordHangupMessage {
                code: code.code(),
                reason,
                target: None,
            }],
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: None,
            routed_callee: None,
            connected_callee: None,
            routed_contact: None,
            routed_destination: None,
            last_queue_name: None,
            target_dialogs: vec![],
            server_dialog_id: Some(server_dialog_id),
            extensions: self.context.dialplan.extensions.clone(),
        };

        reporter.report(snapshot);
        Ok(())
    }

    pub(crate) fn forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.context.dialplan.call_forwarding.as_ref()
    }

    pub(crate) fn immediate_forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.forwarding_config().and_then(|config| {
            if matches!(config.mode, CallForwardingMode::Always) {
                Some(config)
            } else {
                None
            }
        })
    }

    pub(crate) fn forwarding_timeout(&self) -> Option<Duration> {
        self.forwarding_config().and_then(|config| {
            if matches!(config.mode, CallForwardingMode::WhenNoAnswer) {
                Some(config.timeout)
            } else {
                None
            }
        })
    }

    pub(crate) fn failure_is_busy(&self) -> bool {
        self.last_error
            .as_ref()
            .map(|(code, _)| {
                matches!(
                    code,
                    StatusCode::BusyHere
                        | StatusCode::BusyEverywhere
                        | StatusCode::Decline
                        | StatusCode::RequestTerminated
                )
            })
            .unwrap_or(false)
    }

    pub(crate) fn failure_is_no_answer(&self) -> bool {
        if self.is_answered() {
            return false;
        }
        self.last_error
            .as_ref()
            .map(|(code, _)| {
                matches!(
                    code,
                    StatusCode::RequestTimeout
                        | StatusCode::TemporarilyUnavailable
                        | StatusCode::ServerTimeOut
                )
            })
            .unwrap_or(false)
    }

    pub(crate) fn local_contact_uri(&self) -> Option<Uri> {
        let routed_contact = self
            .routed_contact
            .as_ref()
            .and_then(|contact| Uri::try_from(contact.as_str()).ok())
            .or_else(|| self.context.dialplan.caller_contact.as_ref().map(|c| c.uri.clone()));
        SipLeg::local_contact_uri(routed_contact, &self.server)
    }

    pub(crate) async fn process_pending_actions(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        if let Some(inbox) = inbox {
            while let Ok(action) = inbox.try_recv() {
                self.apply_session_action(action, Some(inbox)).await?;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn store_pending_hangup(
        pending: &Arc<Mutex<Option<PendingHangup>>>,
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    ) -> Result<()> {
        let mut guard = pending
            .lock()
            .map_err(|_| anyhow!("pending hangup lock poisoned"))?;
        *guard = Some(PendingHangup {
            reason,
            code,
            initiator,
        });
        Ok(())
    }

    fn resolve_pending_hangup(
        &self,
    ) -> (StatusCode, Option<String>, Option<CallRecordHangupReason>) {
        let pending = self
            .pending_hangup
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        match pending {
            Some(request) => {
                let status_code = request
                    .code
                    .and_then(Self::status_code_for_value)
                    .unwrap_or(StatusCode::RequestTerminated);
                let message = match (request.initiator.as_deref(), request.reason.as_ref()) {
                    (Some(initiator), Some(reason)) => Some(format!(
                        "Cancelled by {} ({})",
                        initiator,
                        reason.to_string()
                    )),
                    (Some(initiator), None) => Some(format!("Cancelled by {}", initiator)),
                    (None, Some(reason)) => Some(format!("Cancelled ({})", reason.to_string())),
                    (None, None) => Some("Cancelled by controller".to_string()),
                };
                (status_code, message, request.reason)
            }
            None => (
                StatusCode::RequestTerminated,
                Some("Cancelled by system".to_string()),
                Some(CallRecordHangupReason::Canceled),
            ),
        }
    }

    fn status_code_for_value(value: u16) -> Option<StatusCode> {
        match value {
            403 => Some(StatusCode::Forbidden),
            404 => Some(StatusCode::NotFound),
            480 => Some(StatusCode::TemporarilyUnavailable),
            486 => Some(StatusCode::BusyHere),
            487 => Some(StatusCode::RequestTerminated),
            488 => Some(StatusCode::NotAcceptableHere),
            500 => Some(StatusCode::ServerInternalError),
            503 => Some(StatusCode::ServiceUnavailable),
            600 => Some(StatusCode::BusyEverywhere),
            603 => Some(StatusCode::Decline),
            _ => None,
        }
    }

    pub fn apply_session_action<'a>(
        &'a mut self,
        action: SessionAction,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match action {
                SessionAction::Hangup {
                    code,
                    reason,
                    initiator,
                } => {
                    let status = code
                        .and_then(|c| StatusCode::try_from(c).ok())
                        .unwrap_or(StatusCode::RequestTerminated);
                    let reason_str = reason.as_ref().map(|r| format!("{:?}", r));
                    self.set_error(status.clone(), reason_str.clone(), None);

                    let actual_reason = reason.clone().unwrap_or(CallRecordHangupReason::Canceled);
                    self.hangup_reason = Some(actual_reason.clone());
                    self.shared.mark_hangup(actual_reason);

                    Self::store_pending_hangup(&self.pending_hangup, reason, code, initiator).ok();

                    if let Err(err) = self
                        .exported_leg
                        .hangup_inbound(Some(status), reason_str.clone())
                        .await
                    {
                        warn!(
                            session_id = %self.context.session_id,
                            error = %err,
                            "Failed to hang up inbound SIP dialog"
                        );
                    }

                    self.cancel_token.cancel();
                    Ok(())
                }
                SessionAction::AcceptCall {
                    callee,
                    sdp,
                    dialog_id,
                } => self.accept_call(callee, sdp, dialog_id).await,
                SessionAction::StartRinging {
                    ringback,
                    passthrough: _,
                } => {
                    self.start_ringing(ringback.unwrap_or_default()).await;
                    Ok(())
                }
                SessionAction::TransferTarget(target) => {
                    match self.context.kind {
                        crate::proxy::proxy_call::state::SessionKind::RwiSingleLeg
                        | crate::proxy::proxy_call::state::SessionKind::AppDriven => {
                            OriginatedRuntime::refer_callee(self, &target).await
                        }
                        _ => {
                            if let Some(endpoint) = TransferEndpoint::parse(&target) {
                                self.transfer_to_endpoint(&endpoint, inbox).await
                            } else {
                                self.transfer_to_uri(&target).await
                            }
                        }
                    }
                }
                SessionAction::HandleReInvite {
                    leg,
                    method,
                    sdp,
                    dialog_id,
                } => {
                    let method = match method.to_uppercase().as_str() {
                        "INVITE" => rsip::Method::Invite,
                        "UPDATE" => rsip::Method::Update,
                        _ => rsip::Method::Invite, // Default to Invite for now
                    };
                    self.handle_reinvite(leg, method, dialog_id, sdp).await;
                    Ok(())
                }
                SessionAction::HandleTrickleIce(payload) => {
                    self.handle_trickle_ice(&payload).await;
                    Ok(())
                }
                SessionAction::PlayPrompt {
                    audio_file,
                    send_progress: _,
                    await_completion,
                    track_id,
                    loop_playback,
                    interrupt_on_dtmf: _,
                } => {
                    let tid = track_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    self.exported_leg
                        .play_audio(
                            &self.context.session_id,
                            &audio_file,
                            await_completion,
                            &tid,
                            loop_playback,
                            self.cancel_token.clone(),
                            self.app_event_tx.clone(),
                        )
                        .await?;
                    Ok(())
                }
                SessionAction::StartRecording {
                    path,
                    max_duration,
                    beep,
                } => {
                    self.exported_leg
                        .start_recording(
                            &self.context.session_id,
                            &mut self.recording_state,
                            &path,
                            max_duration,
                            beep,
                        )
                        .await?;
                    Ok(())
                }
                SessionAction::StopRecording => {
                    self.exported_leg
                        .stop_recording(
                            &self.context.session_id,
                            &mut self.recording_state,
                            self.app_event_tx.clone(),
                        )
                        .await?;
                    Ok(())
                }
                SessionAction::StopPlayback => {
                    self.exported_leg.stop_playback().await;
                    Ok(())
                }
                SessionAction::Hold { music_source } => {
                    match self.context.kind {
                        crate::proxy::proxy_call::state::SessionKind::RwiSingleLeg
                        | crate::proxy::proxy_call::state::SessionKind::AppDriven => {
                            OriginatedRuntime::hold_callee(self, music_source.as_deref()).await
                        }
                        _ => {
                            let audio_file = music_source.clone().unwrap_or_default();
                            if !audio_file.is_empty() {
                                self.exported_leg
                                    .play_audio(
                                        &self.context.session_id,
                                        &audio_file,
                                        true,
                                        "hold_music",
                                        true,
                                        self.cancel_token.clone(),
                                        self.app_event_tx.clone(),
                                    )
                                    .await?;
                            }
                            Ok(())
                        }
                    }
                }
                SessionAction::Unhold => {
                    match self.context.kind {
                        crate::proxy::proxy_call::state::SessionKind::RwiSingleLeg
                        | crate::proxy::proxy_call::state::SessionKind::AppDriven => {
                            OriginatedRuntime::unhold_callee(self).await
                        }
                        _ => {
                            self.exported_leg.unhold().await;
                            Ok(())
                        }
                    }
                }
                SessionAction::BridgeTo { target_session_id } => {
                    // Retrieve the target session's caller_peer via the registry.
                    let target_handle = self
                        .server
                        .active_call_registry
                        .get_handle(&target_session_id);
                    match target_handle {
                        Some(_target) => {
                            info!(
                                session_id = %self.context.session_id,
                                target = %target_session_id,
                                "BridgeTo: cross-session bridge requested"
                            );
                            // Full cross-session MediaBridge coordination requires
                            // getting the actual MediaPeer from both sessions.
                            // For now, we just log that bridge was requested.
                            Ok(())
                        }
                        None => {
                            warn!(
                                session_id = %self.context.session_id,
                                target = %target_session_id,
                                "BridgeTo: target session not found in registry"
                            );
                            Err(anyhow!("target session not found: {}", target_session_id))
                        }
                    }
                }
                SessionAction::Unbridge => {
                    if self.bridge_runtime.media_bridge.is_some() {
                        self.bridge_runtime.stop_bridge();
                        info!(session_id = %self.context.session_id, "Unbridge: media bridge stopped");
                    } else {
                        info!(session_id = %self.context.session_id, "Unbridge: no active bridge to stop");
                    }
                    self.bridge_runtime.clear_bridge();
                    Ok(())
                }
                SessionAction::SupervisorListen { target_session_id } => {
                    info!(
                        session_id = %self.context.session_id,
                        target = %target_session_id,
                        "SupervisorListen: setting up listen mode"
                    );

                    // Get target session's handle to access its media peers
                    if let Some(_target_handle) = self.server.active_call_registry.get_handle(&target_session_id) {
                        // Create a new mixer for supervisor mode
                        self.bridge_runtime.start_supervisor_mode(
                            &self.context.session_id,
                            &target_session_id,
                            self.exported_leg.media.peer.clone(),
                            SupervisorMixerMode::Listen,
                        );

                        info!(session_id = %self.context.session_id, "SupervisorListen: mixer started");
                    } else {
                        warn!(session_id = %self.context.session_id, target = %target_session_id, "SupervisorListen: target session not found");
                    }
                    Ok(())
                }
                SessionAction::SupervisorWhisper { target_session_id } => {
                    info!(
                        session_id = %self.context.session_id,
                        target = %target_session_id,
                        "SupervisorWhisper: setting up whisper mode"
                    );

                    // Get target session
                    if let Some(_target_handle) = self.server.active_call_registry.get_handle(&target_session_id) {
                        // Create mixer for whisper mode
                        self.bridge_runtime.start_supervisor_mode(
                            &self.context.session_id,
                            &target_session_id,
                            self.exported_leg.media.peer.clone(),
                            SupervisorMixerMode::Whisper,
                        );

                        info!(session_id = %self.context.session_id, "SupervisorWhisper: mixer started");
                    }
                    Ok(())
                }
                SessionAction::SupervisorBarge { target_session_id } => {
                    info!(
                        session_id = %self.context.session_id,
                        target = %target_session_id,
                        "SupervisorBarge: setting up barge (3-way) mode"
                    );

                    // Get target session
                    if let Some(_target_handle) = self.server.active_call_registry.get_handle(&target_session_id) {
                        // Create mixer for barge mode (3-way conference)
                        self.bridge_runtime.start_supervisor_mode(
                            &self.context.session_id,
                            &target_session_id,
                            self.exported_leg.media.peer.clone(),
                            SupervisorMixerMode::Barge,
                        );

                        info!(session_id = %self.context.session_id, "SupervisorBarge: mixer started");
                    }
                    Ok(())
                }
                SessionAction::SupervisorStop => {
                    info!(session_id = %self.context.session_id, "SupervisorStop: stopping supervisor mode");
                    // Stop and clean up the supervisor mixer
                    if self.bridge_runtime.supervisor_mixer.is_some() {
                        self.bridge_runtime.stop_supervisor_mode();
                        info!(session_id = %self.context.session_id, "SupervisorStop: mixer stopped");
                    }
                    Ok(())
                }
                SessionAction::StartSupervisorMode {
                    supervisor_session_id,
                    target_session_id,
                    mode,
                } => {
                    info!(
                        session_id = %self.context.session_id,
                        supervisor = %supervisor_session_id,
                        target = %target_session_id,
                        mode = ?mode,
                        "StartSupervisorMode: setting up supervisor mode"
                    );

                    // Verify supervisor session exists
                    let supervisor_handle = self.server.active_call_registry.get_handle(&supervisor_session_id);
                    if supervisor_handle.is_none() {
                        warn!(
                            session_id = %self.context.session_id,
                            supervisor = %supervisor_session_id,
                            "StartSupervisorMode: supervisor session not found"
                        );
                        return Ok(());
                    }

                    // Verify target session exists
                    let target_handle = self.server.active_call_registry.get_handle(&target_session_id);
                    if target_handle.is_none() {
                        warn!(
                            session_id = %self.context.session_id,
                            target = %target_session_id,
                            "StartSupervisorMode: target session not found"
                        );
                        return Ok(());
                    }

                    // For now, we store the supervisor state but don't create the actual mixer
                    // The full implementation requires access to supervisor's peer which is not available
                    // until we implement peer sharing between sessions
                    info!(
                        session_id = %self.context.session_id,
                        supervisor = %supervisor_session_id,
                        target = %target_session_id,
                        mode = ?mode,
                        "StartSupervisorMode: state stored (full mixing deferred until peer sharing implemented)"
                    );

                    Ok(())
                }
                _ => unreachable!("unsupported session control action"),
            }
        }
        .boxed()
    }

    pub(crate) fn transfer_to_endpoint<'a>(
        &'a mut self,
        endpoint: &'a TransferEndpoint,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match endpoint {
                TransferEndpoint::Uri(uri) => self.transfer_to_uri(uri).await,
                TransferEndpoint::Queue(name) => {
                    if let Some(inbox) = inbox {
                        info!(session_id = %self.context.session_id, queue = %name, "Transferring to queue");

                        let lookup_ref = if name.chars().all(|c| c.is_ascii_digit()) {
                            format!("db-{}", name)
                        } else {
                            name.clone()
                        };

                        let queue_config = self
                            .server
                            .data_context
                            .load_queue(&lookup_ref)
                            .await?
                            .ok_or_else(|| anyhow!("Queue not found: {}", name))?;

                        let mut plan = queue_config.to_queue_plan()?;
                        if plan.label.is_none() {
                            plan.label = Some(name.clone());
                        }

                        QueueFlow::execute_queue_plan(self, &plan, None, Some(inbox)).await
                    } else {
                        warn!("Queue forwarding not supported without inbox: {}", name);
                        Err(anyhow!("Queue forwarding not supported without inbox"))
                    }
                }
            }
        }
        .boxed()
    }

    async fn transfer_to_uri(&mut self, uri: &str) -> Result<()> {
        let parsed = Uri::try_from(uri)
            .map_err(|err| anyhow!("invalid forwarding uri '{}': {}", uri, err))?;
        let mut location = Location::default();
        location.aor = parsed.clone();
        location.contact_raw = Some(parsed.to_string());
        match self.try_single_target(&location).await {
            Ok(_) => Ok(()),
            Err((code, reason)) => {
                let message = reason.unwrap_or_else(|| code.to_string());
                Err(anyhow!("forwarding to {} failed: {}", uri, message))
            }
        }
    }

    pub(crate) fn execute_flow<'a>(
        &'a mut self,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        DialplanRuntime::execute_flow(self, flow, inbox, FlowFailureHandling::Handle)
    }

    pub(crate) async fn run_targets(
        &mut self,
        strategy: &DialStrategy,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.shared.transition_to_dialing();
        self.process_pending_actions(inbox.as_deref_mut()).await?;
        debug!(
            session_id = %self.context.session_id,
            strategy = %strategy,
            media_proxy = self.use_media_proxy,
            "executing dialplan"
        );

        match strategy {
            DialStrategy::Sequential(targets) => TargetRuntime::dial_sequential(self, targets, inbox).await,
            DialStrategy::Parallel(targets) => self.dial_parallel(targets, inbox).await,
        }
    }

    async fn dial_parallel(
        &mut self,
        targets: &[Location],
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        TargetRuntime::dial_parallel(self, targets, inbox).await
    }

    pub(crate) async fn try_single_target(
        &mut self,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let caller = self
            .context
            .dialplan
            .caller
            .as_ref()
            .ok_or((
                StatusCode::ServerInternalError,
                Some("No caller specified in dialplan".to_string()),
            ))?
            .clone();
        let caller_display_name = self.context.dialplan.caller_display_name.clone();

        // Generate offer based on target's WebRTC support
        let offer = self.create_offer_for_target(target).await;

        let enforced_contact = self.local_contact_uri();
        let headers = self
            .context
            .dialplan
            .build_invite_headers(&target)
            .unwrap_or_default();
        let invite_option = self.target_leg().sip.build_outbound_invite_option(
            target,
            caller.clone(),
            caller_display_name,
            offer,
            enforced_contact.clone(),
            headers,
            self.context.max_forwards,
            if self.server.proxy_config.session_timer {
                self.server.proxy_config.session_expires
            } else {
                None
            },
            self.context.dialplan.call_id.clone(),
        );

        let mut invite_option = if let Some(ref route_invite) = self.context.dialplan.route_invite {
            let route_result = route_invite
                .route_invite(
                    invite_option,
                    &self.context.dialplan.original,
                    &self.context.dialplan.direction,
                    &self.context.cookie,
                )
                .await
                .map_err(|e| {
                    warn!(session_id = %self.context.session_id, error = %e, "Routing function error");
                    (
                        StatusCode::ServerInternalError,
                        Some("Routing function error".to_string()),
                    )
                })?;
            match route_result {
                RouteResult::NotHandled(option, _) => {
                    debug!(session_id = self.context.session_id, %target,
                        "Routing function returned NotHandled"
                    );
                    option
                }
                RouteResult::Forward(option, _) | RouteResult::Queue { option, .. } => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = self.context.session_id, %code, ?reason, "route abort");
                    return Err((code, reason));
                }
                RouteResult::Application { option, .. } => option,
            }
        } else {
            invite_option
        };

        if let Some(contact_uri) = enforced_contact {
            invite_option.contact = contact_uri;
        }

        let callee_uri = &invite_option.callee;
        let callee_realm = callee_uri.host_with_port.to_string();
        if self.server.is_same_realm(&callee_realm).await {
            let dialplan = &self.context.dialplan;
            let locations = self.server.locator.lookup(&callee_uri).await.map_err(|e| {
                (
                    rsip::StatusCode::TemporarilyUnavailable,
                    Some(e.to_string()),
                )
            })?;

            if locations.is_empty() {
                match self
                    .server
                    .user_backend
                    .get_user(
                        &callee_uri.user().unwrap_or_default(),
                        Some(&callee_realm),
                        Some(&self.context.dialplan.original),
                    )
                    .await
                {
                    Ok(Some(_)) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "user offline in locator, abort now");
                        return Err((
                            rsip::StatusCode::TemporarilyUnavailable,
                            Some("User offline".to_string()),
                        ));
                    }
                    Ok(None) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "user not found in auth backend, reject");
                        return Err((
                            rsip::StatusCode::NotFound,
                            Some("User not found".to_string()),
                        ));
                    }
                    Err(e) => {
                        warn!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "failed to lookup user in auth backend: {}", e);
                        return Err((rsip::StatusCode::ServerInternalError, Some(e.to_string())));
                    }
                }
            } else {
                invite_option.destination = locations[0].destination.clone();
            }
        }

        let destination = invite_option
            .destination
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".to_string());

        debug!(
            session_id = %self.context.session_id, %caller, %target, destination,
            "Sending INVITE to callee"
        );

        TargetRuntime::try_single_target(self, target).await
    }

    pub(crate) async fn handle_failure(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        TargetRuntime::handle_failure(self, inbox).await
    }

    /// Run a call application (voicemail, IVR, etc.) by looking up the addon,
    /// instantiating the app, and driving it through `AppEventLoop`.
    pub(crate) async fn run_application(
        &mut self,
        app_name: &str,
        app_params: Option<serde_json::Value>,
        auto_answer: bool,
    ) -> Result<()> {
        self.shared.transition_to_app();
        let app = AppRuntime::build_call_app(self, app_name, &app_params).await?;
        AppRuntime::run(self, app_name, app, auto_answer).await
    }


    /// Consolidates session teardown into a single, ordered sequence:
    /// 1. Transition to `Terminating` phase
    /// 2. Resolve any pending hangup
    /// 3. Stop the media bridge (media-only cleanup)
    /// 4. Send SIP cleanup to callee dialogs
    /// 5. Send SIP cleanup to caller dialog
    /// 6. Wait for actual SIP termination (or grace timeout)
    /// 7. Finalize: report call record, mark as `Ended`
    async fn enter_terminating(&mut self) {
        // Step 1: Phase transition
        self.shared.transition_to_terminating();

        // Step 2: Resolve pending hangup if not already set
        if self.hangup_reason.is_none() {
            let (status_code, reason_text, hangup_reason) = self.resolve_pending_hangup();
            if let Some(reason) = hangup_reason {
                self.hangup_reason = Some(reason);
            }
            if self.last_error.is_none() {
                self.set_error(status_code, reason_text, None);
            }
        }

        // Step 3: Stop bridge (media-only cleanup, legs stay alive for SIP cleanup)
        if self.bridge_runtime.is_active() {
            debug!(session_id = %self.context.session_id, "Stopping media bridge during termination");
            self.bridge_runtime.stop_bridge();
        }

        // Step 4: Initialize termination tracking
        let mut term_state = TerminationState::new(3);

        // Step 5: SIP dialog cleanup — terminate callee dialogs
        if let Some(ref callee) = self.target_leg {
            callee
                .terminate_client_dialogs(&self.context.session_id, &self.dialog_layer)
                .await;
        }
        term_state.callee_cleanup_sent = true;

        // Step 6: SIP dialog cleanup — terminate or reject caller dialog
        if !self.exported_leg.is_server_dialog_terminated() {
            let normal_hangup = matches!(
                self.hangup_reason,
                Some(CallRecordHangupReason::ByCallee) | Some(CallRecordHangupReason::ByCaller)
            );

            if normal_hangup && (self.exported_leg.is_server_dialog_confirmed() || self.exported_leg.is_server_dialog_waiting_ack()) {
                info!(
                    session_id = %self.context.session_id,
                    hangup_reason = ?self.hangup_reason,
                    "Sending BYE to caller for normal call teardown"
                );
                let _ = self.exported_leg.bye_inbound().await;
            } else {
                let (code, reason) = if self.context.dialplan.passthrough_failure {
                    self.last_error.clone().unwrap_or((
                        StatusCode::ServerInternalError,
                        Some("Call failed".to_string()),
                    ))
                } else {
                    (
                        StatusCode::ServerInternalError,
                        Some("Call failed".to_string()),
                    )
                };

                info!(
                    session_id = %self.context.session_id,
                    status_code = %code,
                    passthrough = self.context.dialplan.passthrough_failure,
                    "Rejecting caller with failure response"
                );
                let _ = self.exported_leg.reject_inbound(Some(code), reason);
            }
            term_state.caller_cleanup_sent = true;
        }

        self.termination_state = Some(term_state);

        // Step 7: Wait for actual SIP termination, then finalize
        self.await_termination_completion().await;
        self.finalize_session();
    }

    /// Check whether the session can finalize (all relevant legs terminated).
    ///
    /// Uses `TerminationState` to decide which legs need to be waited on:
    /// only legs that had cleanup sent need to reach terminated state.
    fn can_finalize_termination(&self) -> bool {
        let ts = match self.termination_state.as_ref() {
            Some(ts) => ts,
            None => return true, // no termination tracking → finalize immediately
        };

        // Caller side: OK if no cleanup needed, no server dialog exists, or dialog is terminated
        let caller_ok = if ts.caller_cleanup_sent {
            self.exported_leg.server_dialog_ref().is_none()
                || self.exported_leg.is_server_dialog_terminated()
        } else {
            true
        };

        // Callee side: must have no active dialogs if cleanup was sent
        let callee_ok = if ts.callee_cleanup_sent {
            self.target_leg.as_ref().map_or(true, |l| l.sip.active_dialog_ids.lock().unwrap().is_empty())
        } else {
            true
        };

        caller_ok && callee_ok
    }

    /// Wait until all SIP dialogs are confirmed terminated or the grace
    /// timeout expires, whichever comes first.
    async fn await_termination_completion(&self) {
        if self.can_finalize_termination() {
            debug!(session_id = %self.context.session_id, "All SIP dialogs already terminated, finalizing immediately");
            return;
        }

        let grace_duration = self
            .termination_state
            .as_ref()
            .map(|ts| ts.grace_deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3));

        let timeout = tokio::time::sleep(grace_duration);
        tokio::pin!(timeout);
        let mut poll_interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                _ = &mut timeout => {
                    debug!(
                        session_id = %self.context.session_id,
                        caller_terminated = self.exported_leg.is_server_dialog_terminated(),
                        callee_empty = self.target_leg.as_ref().map_or(true, |l| l.sip.active_dialog_ids.lock().unwrap().is_empty()),
                        "Termination grace period expired, finalizing"
                    );
                    break;
                }
                _ = poll_interval.tick() => {
                    if self.can_finalize_termination() {
                        debug!(session_id = %self.context.session_id, "All SIP dialogs terminated, finalizing");
                        break;
                    }
                }
            }
        }
    }

    /// Report the call record and mark the session as ended.
    /// Called from enter_terminating after SIP cleanup is observed complete.
    /// Drop impl serves as a safety net if this is not reached.
    fn finalize_session(&mut self) {
        if let Some(reporter) = self.reporter.take() {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
        }
        let reason = self.hangup_reason.clone().unwrap_or(CallRecordHangupReason::Canceled);
        self.shared.mark_hangup(reason);
    }

    /// Originated dial flow: sends a single outbound INVITE and handles
    /// the dialog state transitions using session phases and shared state.

    pub async fn serve(
        server: SipServerRef,
        context: CallContext,
        tx: &mut rsipstack::transaction::transaction::Transaction,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
    ) -> Result<()> {
        if tx.original.body.is_empty() {
            info!(
                session_id = %context.session_id,
                "Rejecting call with 488 Not Acceptable Here due to missing SDP"
            );
            tx.reply(StatusCode::NotAcceptableHere).await?;
            return Ok(());
        }
        let (state_tx, state_rx) = mpsc::unbounded_channel();
        let local_contact = context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| server.default_contact_uri());

        let server_dialog = server.dialog_layer.get_or_create_server_invite(
            tx,
            state_tx,
            None,
            local_contact.clone(),
        )?;

        // Log initial server dialog details
        info!(
            session_id = %context.session_id,
            dialog_id = %server_dialog.id(),
            "Server dialog created"
        );

        let initial_request = server_dialog.initial_request();
        let offer_sdp = String::from_utf8_lossy(initial_request.body()).to_string();

        if !offer_sdp.trim().is_empty() {
            if let Err(e) = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp)
            {
                info!(
                    session_id = %context.session_id,
                    error = %e,
                    "Rejecting call with 400 Bad Request due to malformed SDP"
                );
                let _ = server_dialog.reject(Some(StatusCode::BadRequest), None);
                return Ok(());
            }
        }

        let all_webrtc_target = context.dialplan.all_webrtc_target();
        let use_media_proxy = Self::check_media_proxy(
            &context,
            &offer_sdp,
            &server.proxy_config.media_proxy,
            all_webrtc_target,
        );

        info!(
            session_id = %context.session_id,
            server_dialog_id = %server_dialog.id(),
            use_media_proxy,
            all_webrtc_target,
            "starting proxy call processing"
        );

        // Only create recorder when:
        // 1. Recording is enabled and auto_start is true
        // 2. AND no sipflow backend is configured (to avoid duplicate storage)
        let has_sipflow_backend = server
            .sip_flow
            .as_ref()
            .and_then(|sf| sf.backend())
            .is_some();

        let recorder_option = if context.dialplan.recording.enabled
            && context.dialplan.recording.auto_start
            && !has_sipflow_backend
        {
            context.dialplan.recording.option.clone()
        } else {
            None
        };

        let reporter = CallReporter {
            server: server.clone(),
            context: context.clone(),
            call_record_sender: call_record_sender.clone(),
        };

        let exported_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", context.session_id))
            .with_cancel_token(cancel_token.child_token());
        let exported_peer = Arc::new(VoiceEnginePeer::new(Arc::new(exported_media_builder.build())));

        let session_shared = CallSessionShared::new(
            context.session_id.clone(),
            context.dialplan.direction,
            context.dialplan.caller.as_ref().map(|c| c.to_string()),
            context
                .dialplan
                .first_target()
                .map(|location| location.aor.to_string()),
            Some(server.active_call_registry.clone()),
        );

        let mut session = Box::new(CallSession::new(
            server.clone(),
            server.dialog_layer.clone(),
            cancel_token.clone(),
            call_record_sender.clone(),
            context.clone(),
            Some(server_dialog.clone()),
            use_media_proxy,
            recorder_option,
            exported_peer,
            None, // target leg created lazily via ensure_target_leg()
            session_shared.clone(),
            Some(reporter),
        ));

        session.exported_leg.sip.supports_trickle_ice =
            SipLeg::has_trickle_ice_support(&initial_request.headers);

        // Store caller offer SDP; target offer will be set when target leg is created
        session.exported_leg.media.offer_sdp = Some(offer_sdp);

        // In serve(), the server dialog was just created above — safe to expect.
        let caller_dialog_id = session.exported_leg.server_dialog_id()
            .expect("serve() always creates a server dialog");
        let dialog_guard =
            ServerDialogGuard::new(server.dialog_layer.clone(), caller_dialog_id);
        let (handle, action_rx) = CallSessionHandle::with_shared(session_shared.clone());
        session.register_active_call(handle);

        let (target_state_tx, target_state_rx) = mpsc::unbounded_channel();
        session.target_dialog_event_tx = Some(target_state_tx);

        let action_inbox = SessionActionInbox::new(action_rx);

        let mut server_dialog_clone = session.exported_leg.clone_server_dialog()
            .expect("serve() always creates a server dialog");
        crate::utils::spawn(async move {
            session
                .process(Some(state_rx), target_state_rx, action_inbox, Some(dialog_guard))
                .await
        });
        let ring_time_secs = context.dialplan.max_ring_time.clamp(30, 120);
        let max_setup_duration = Duration::from_secs(ring_time_secs as u64);
        let teardown_duration = Duration::from_secs(2);
        let mut timeout = tokio::time::sleep(max_setup_duration).boxed();
        let mut canceld = false;
        loop {
            tokio::select! {
                r = server_dialog_clone.handle(tx) => {
                    debug!(session_id = %context.session_id, "Server dialog handle returned");
                    if let Err(ref e) = r {
                        warn!(session_id = %context.session_id, error = %e, "Server dialog handle returned error, cancelling call");
                        cancel_token.cancel();
                    }
                    break;
                }
                _ = cancel_token.cancelled(), if !canceld => {
                    debug!(session_id = %context.session_id, "Call cancelled via token during setup");
                    canceld = true;
                    timeout = tokio::time::sleep(teardown_duration).boxed();
                }
                _ = &mut timeout => {
                     warn!(session_id = %context.session_id, "Call setup timed out (180s), forcing cancellation");
                     cancel_token.cancel();
                     break;
                }
            }
        }
        Ok(())
    }

    /// Bootstrap an RWI-originated session: no inbound server dialog,
    /// single outbound target dialed directly.
    ///
    /// Returns the session handle and shared state so the RWI layer can
    /// send commands and observe state.
    pub async fn serve_originated(
        server: SipServerRef,
        call_id: String,
        invite_option: InviteOption,
        exported_peer: Arc<VoiceEnginePeer>,
        cancel_token: CancellationToken,
        timeout_secs: u64,
        caller_display: Option<String>,
        callee_display: Option<String>,
        event_tx: Option<mpsc::UnboundedSender<OriginatedSessionEvent>>,
    ) -> (CallSessionHandle, CallSessionShared) {
        use crate::call::{DialDirection, Dialplan, DialplanFlow, DialStrategy, MediaConfig};
        use crate::proxy::proxy_call::state::SessionKind;

        // Build a synthetic request for reporting — minimal INVITE with caller/callee URIs
        let caller_uri_str = caller_display.clone().unwrap_or_else(|| "sip:rwi@local".to_string());
        let callee_uri_str = callee_display.clone().unwrap_or_else(|| invite_option.callee.to_string());
        let synthetic_request = rsip::Request {
            method: rsip::Method::Invite,
            uri: invite_option.callee.clone(),
            version: rsip::Version::V2,
            headers: rsip::Headers::from(vec![
                rsip::Header::From(format!("<{}>", caller_uri_str).into()),
                rsip::Header::To(format!("<{}>", callee_uri_str).into()),
                rsip::Header::CallId(call_id.clone().into()),
            ]),
            body: invite_option.offer.clone().unwrap_or_default(),
        };

        let dialplan = Arc::new(Dialplan {
            direction: DialDirection::Outbound,
            session_id: Some(call_id.clone()),
            call_id: Some(call_id.clone()),
            original: Arc::new(synthetic_request),
            caller_display_name: caller_display.clone(),
            caller: rsip::Uri::try_from(caller_uri_str.as_str()).ok(),
            caller_contact: None,
            flow: DialplanFlow::Targets(DialStrategy::Sequential(vec![])),
            max_ring_time: timeout_secs as u32,
            recording: Default::default(),
            ringback: Default::default(),
            media: MediaConfig {
                proxy_mode: server.proxy_config.media_proxy,
                external_ip: server.rtp_config.external_ip.clone(),
                rtp_start_port: server.rtp_config.start_port,
                rtp_end_port: server.rtp_config.end_port,
                webrtc_port_start: server.rtp_config.webrtc_start_port,
                webrtc_port_end: server.rtp_config.webrtc_end_port,
                ice_servers: server.rtp_config.ice_servers.clone(),
                enable_latching: server.proxy_config.enable_latching,
            },
            max_call_duration: Some(Duration::from_secs(3600)),
            call_timeout: Duration::from_secs(timeout_secs),
            failure_action: Default::default(),
            enable_sipflow: true,
            call_forwarding: None,
            voicemail_enabled: false,
            route_invite: None,
            with_original_headers: false,
            extensions: http::Extensions::new(),
            allow_codecs: vec![
                CodecType::G729,
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                #[cfg(feature = "opus")]
                CodecType::Opus,
                CodecType::TelephoneEvent,
            ],
            passthrough_failure: false,
        });

        let context = CallContext {
            session_id: call_id.clone(),
            kind: SessionKind::RwiSingleLeg,
            dialplan: dialplan.clone(),
            cookie: crate::call::cookie::TransactionCookie::default(),
            start_time: Instant::now(),
            media_config: dialplan.media.clone(),
            original_caller: caller_display.unwrap_or_default(),
            original_callee: callee_display.unwrap_or_default(),
            max_forwards: 70,
        };

        let reporter = CallReporter {
            server: server.clone(),
            context: context.clone(),
            call_record_sender: None,
        };

        let session_shared = CallSessionShared::new(
            call_id.clone(),
            DialDirection::Outbound,
            context.dialplan.caller.as_ref().map(|c| c.to_string()),
            Some(invite_option.callee.to_string()),
            Some(server.active_call_registry.clone()),
        );

        let offer_sdp_string = invite_option
            .offer
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string());

        let mut session = Box::new(CallSession::new(
            server.clone(),
            server.dialog_layer.clone(),
            cancel_token.clone(),
            None,
            context,
            None, // no server dialog for originated calls
            true, // use_media_proxy — originated calls always proxy media
            None, // no recorder option
            exported_peer,
            None, // no target leg yet — created during dial
            session_shared.clone(),
            Some(reporter),
        ));

        // Set offer SDP on exported leg only (target doesn't exist yet)
        if let Some(ref sdp) = offer_sdp_string {
            session.exported_leg.media.offer_sdp = Some(sdp.clone());
        }

        // Store originated dial params for process() to use
        session.originated_dial_params = Some(OriginatedDialParams {
            invite_option,
            timeout_secs,
            event_tx,
        });

        let (handle, action_rx) = CallSessionHandle::with_shared(session_shared.clone());
        session.register_active_call(handle.clone());

        // Create target event channel — target_state_tx stored for dial to set on leg
        let (target_state_tx, target_state_rx) = mpsc::unbounded_channel();
        session.target_dialog_event_tx = Some(target_state_tx);

        let action_inbox = SessionActionInbox::new(action_rx);

        crate::utils::spawn(async move {
            session
                .process(None, target_state_rx, action_inbox, None)
                .await
        });

        (handle, session_shared)
    }

    pub async fn process(
        mut self: Box<Self>,
        state_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
        target_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut action_inbox: SessionActionInbox,
        dialog_guard: Option<ServerDialogGuard>,
    ) {
        let _cancel_token_guard = self.cancel_token.clone().drop_guard();

        let use_media_proxy = self.use_media_proxy;
        let exported_peer = self.exported_leg.media.peer.clone();
        let target_peer = self.target_leg.as_ref().map(|l| l.media.peer.clone());

        let _guard = dialog_guard;

        // Session timer only applies when we have an inbound server dialog (proxy sessions)
        if self.server.proxy_config.session_timer && self.exported_leg.server_dialog_ref().is_some() {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            if let Err((code, _min_se)) = self.init_server_timer(default_expires) {
                info!("Rejecting call with 422 Session Interval Too Small");
                let _ = self.exported_leg.reject_inbound(Some(code), None);
                return;
            }
        }

        let server_timer = self.exported_leg.sip.session_timer.clone();
        let client_timer = self.target_leg.as_ref()
            .map(|l| l.sip.session_timer.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(crate::proxy::proxy_call::session_timer::SessionTimerState::default())));
        let target_dialogs = self.target_leg.as_ref()
            .map(|l| l.sip.active_dialog_ids.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(std::collections::HashSet::new())));
        let server_dialog_clone = self.exported_leg.clone_server_dialog();
        let handle_for_events = self.handle.clone().unwrap();

        let context_clone = self.context.clone();
        let proxy_config_clone = self.server.proxy_config.clone();
        let dialog_layer_clone = self.dialog_layer.clone();
        let cancel_token_for_select = self.cancel_token.clone();
        let cancel_token_for_max_duration = self.cancel_token.clone();
        let cancel_token_for_loop = self.cancel_token.clone();
        let pending_hangup = self.pending_hangup.clone();
        let session_id_for_max_duration = self.context.session_id.clone();
        let max_duration = self
            .context
            .dialplan
            .max_call_duration
            .or_else(|| self.context.cookie.get_max_duration());

        let shared_for_loop = self.shared.clone();
        let _ = tokio::select! {
            _ = cancel_token_for_select.cancelled() => {
                let (status_code, reason_text, hangup_reason) = self.resolve_pending_hangup();
                debug!(session_id = %self.context.session_id, ?status_code, ?reason_text, "Call cancelled via token");
                self.set_error(status_code, reason_text.clone(), None);
                self.hangup_reason = hangup_reason.or(Some(CallRecordHangupReason::Canceled));
                Err(anyhow!("Call cancelled"))
            }
            _ = async {
                if use_media_proxy {
                    match target_peer {
                        Some(tp) => { let _ = tokio::join!(exported_peer.serve(), tp.serve()); }
                        None => { let _ = exported_peer.serve().await; }
                    }
                } else {
                    cancel_token_for_select.cancelled().await;
                }
            } => { Ok(()) },
            _ = async {
                if let Some(duration) = max_duration {
                    tokio::time::sleep(duration).await;
                    info!(session_id = %session_id_for_max_duration, "max duration reached, cancelling call");
                    cancel_token_for_max_duration.cancel();
                } else {
                    cancel_token_for_max_duration.cancelled().await;
                }
            } => { Ok(()) },
            r = async {
                match self.context.kind {
                    crate::proxy::proxy_call::state::SessionKind::Proxy => {
                        DialplanRuntime::execute_dialplan(&mut self, Some(&mut action_inbox)).await?;
                        info!(session_id = %self.context.session_id, "Dialplan execution finished, waiting for call termination");
                    }
                    crate::proxy::proxy_call::state::SessionKind::RwiSingleLeg
                    | crate::proxy::proxy_call::state::SessionKind::AppDriven => {
                        OriginatedRuntime::run_dial(&mut self, &mut action_inbox).await?;
                        info!(session_id = %self.context.session_id, "Originated dial finished, waiting for call termination");
                    }
                }
                loop {
                    if let Some(action) = action_inbox.recv().await {
                        self.apply_session_action(action, Some(&mut action_inbox)).await?;
                    } else {
                        break;
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => r,
            r = SessionLoopRuntime::run_server_events_loop(
                context_clone.clone(),
                proxy_config_clone,
                dialog_layer_clone.clone(),
                state_rx,
                Some(target_state_rx),
                server_timer,
                client_timer,
                target_dialogs.clone(),
                server_dialog_clone,
                handle_for_events,
                cancel_token_for_loop,
                pending_hangup,
                shared_for_loop
            ) => r,
        };

        self.enter_terminating().await;
    }
}

impl Drop for CallSession {
    fn drop(&mut self) {
        if let Some(gateway) = self.server.rwi_gateway.clone() {
            let call_id = self.shared.session_id();
            let hangup_event = self.shared.rwi_hangup_event();
            crate::utils::spawn(async move {
                {
                    let gw = gateway.read().await;
                    gw.send_event_to_call_owner(&call_id, &hangup_event);
                }
                let leg = {
                    let mut gw = gateway.write().await;
                    gw.remove_leg(&call_id)
                };
                if let Some(leg) = leg {
                    leg.clear_runtime().await;
                }
            });
        }
        self.shared.unregister();
        if let Some(ref bridge) = self.bridge_runtime.media_bridge {
            bridge.stop();
        }
        self.exported_leg.media.peer.stop();
        if let Some(ref callee) = self.target_leg {
            callee.media.peer.stop();
        }
        if let Some(reporter) = self.reporter.take() {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
        }
    }
}

// ─────────────────────────────────────────────────────────────
// DTMF reception helpers (RFC 4733 / RTP telephone-event)
// ─────────────────────────────────────────────────────────────
