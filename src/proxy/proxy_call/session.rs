use crate::call::sip::{DialogStateReceiverGuard, ServerDialogGuard};
use crate::media::mixer::SupervisorMixerMode;
use crate::media::negotiate::CodecInfo;
use crate::proxy::proxy_call::answer_runtime::AnswerRuntime;
use crate::proxy::proxy_call::app_runtime::AppRuntime;
use crate::proxy::proxy_call::bridge_runtime::BridgeRuntime;
use crate::proxy::proxy_call::call_leg::{CallLeg, CallLegDirection};
use crate::proxy::proxy_call::dialplan_runtime::DialplanRuntime;
use crate::proxy::proxy_call::media_endpoint::{BridgeSelection, MediaEndpoint};
use crate::proxy::proxy_call::playback_runtime;
use crate::proxy::proxy_call::queue_flow::QueueFlow;
use crate::proxy::proxy_call::recording_runtime::{self, RecordingState};
use crate::proxy::proxy_call::reporter::CallReporter;
use crate::proxy::proxy_call::session_loop_runtime::SessionLoopRuntime;
use crate::proxy::proxy_call::sip_leg::SipLeg;
use crate::proxy::proxy_call::target_runtime::TargetRuntime;
use crate::proxy::routing::matcher::RouteResourceLookup;
use crate::rwi::call_leg::{RwiCallLeg, RwiCallLegState};
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
                CallContext, CallSessionHandle, CallSessionShared, ProxyCallEvent, SessionAction,
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
    pub callee_dialogs: Vec<DialogId>,
    pub server_dialog_id: DialogId,
    pub extensions: http::Extensions,
}

pub(crate) struct CallSession {
    pub server: SipServerRef,
    pub dialog_layer: Arc<DialogLayer>,
    pub cancel_token: CancellationToken,
    pub call_record_sender: Option<CallRecordSender>,
    pub pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
    pub context: CallContext,
    pub server_dialog: ServerInviteDialog,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub connected_callee: Option<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub shared: CallSessionShared,
    pub caller_leg: CallLeg,
    pub callee_leg: CallLeg,
    pub bridge_runtime: BridgeRuntime,
    pub use_media_proxy: bool,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    pub negotiation_state: NegotiationState,
    pub handle: Option<CallSessionHandle>,
    /// Channel used to deliver [`ControllerEvent`]s to the running `CallApp` event loop.
    /// Populated by [`run_application`] for the lifetime of the app; cleared when the
    /// app exits so stale senders are never accidentally reused.
    pub app_event_tx: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    /// Current recording state (path, start time).
    pub recording_state: RecordingState,
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
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        recorder_option: Option<crate::media::recorder::RecorderOption>,
        caller_peer: Arc<dyn MediaPeer>,
        callee_peer: Arc<dyn MediaPeer>,
        shared: CallSessionShared,
        reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    ) -> Self {
        let initial = server_dialog.initial_request();
        let caller_offer = if initial.body().is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(initial.body()).to_string())
        };

        let session = Self {
            server,
            dialog_layer,
            cancel_token,
            call_record_sender,
            pending_hangup: Arc::new(Mutex::new(None)),
            context,
            server_dialog,
            last_error: None,
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            shared,
            caller_leg: CallLeg::new(CallLegDirection::Inbound, caller_peer, caller_offer),
            callee_leg: CallLeg::new(CallLegDirection::Outbound, callee_peer, None),
            bridge_runtime: BridgeRuntime::new(recorder_option),
            use_media_proxy,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            reporter,
            negotiation_state: NegotiationState::Idle,
            handle: None,
            app_event_tx: None,
            recording_state: None,
        };
        session
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
        if let Some(tx) = &self.callee_leg.sip.dialog_event_tx {
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
        self.callee_leg.sip.dialog_guards.push(guard);
    }

    pub fn init_server_timer(
        &mut self,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
        self.caller_leg
            .sip
            .init_timer_from_initial_request(&self.server_dialog.initial_request(), default_expires)
    }

    pub fn init_client_timer(&mut self, response: &rsip::Response, default_expires: u64) {
        self.callee_leg
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
        self.shared
            .register_dialog(self.server_dialog.id().to_string(), handle.clone());
        self.handle = Some(handle);
        if let Some(handle) = self.handle.clone() {
            let gateway = self.server.rwi_gateway.clone();
            let call_id = self.shared.session_id();
            let peer = self.caller_leg.peer.clone();
            let offer_sdp = self.caller_leg.offer_sdp.clone();
            let answer_sdp = self.caller_leg.answer_sdp.clone();
            let negotiated_audio = self.caller_leg.negotiated_audio.clone();
            let caller = self.routed_caller.clone().or_else(|| {
                (!self.context.original_caller.is_empty()).then(|| self.context.original_caller.clone())
            });
            let callee = self.connected_callee.clone().or_else(|| self.routed_callee.clone()).or_else(|| {
                (!self.context.original_callee.is_empty()).then(|| self.context.original_callee.clone())
            });
            let direction = self.context.dialplan.direction.to_string();
            let ssrc = self.caller_leg.answered_ssrc();
            crate::utils::spawn(async move {
                let Some(gateway) = gateway else {
                    return;
                };
                let leg = {
                    let mut gw = gateway.write().await;
                    if let Some(existing) = gw.get_leg(&call_id) {
                        existing
                    } else {
                        let leg = RwiCallLeg::new_attached(
                            &crate::proxy::active_call_registry::ActiveProxyCallEntry {
                                session_id: call_id.clone(),
                                caller: caller.clone(),
                                callee: callee.clone(),
                                direction: direction.clone(),
                                started_at: chrono::Utc::now(),
                                answered_at: None,
                                status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
                            },
                            handle,
                        );
                        gw.register_leg(call_id.clone(), leg.clone());
                        leg
                    }
                };
                leg.set_peer(peer).await;
                leg.set_offer(offer_sdp).await;
                if let Some(answer_sdp) = answer_sdp {
                    leg.set_answer(answer_sdp).await;
                }
                leg.set_negotiated_media(negotiated_audio.clone(), ssrc).await;
                leg.set_state(if negotiated_audio.is_some() {
                    RwiCallLegState::Answered
                } else {
                    RwiCallLegState::Initializing
                })
                .await;
            });
        }
    }

    pub(crate) async fn sync_rwi_attached_leg(&self) {
        let Some(gateway) = self.server.rwi_gateway.as_ref() else {
            return;
        };
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let call_id = self.shared.session_id();
        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: call_id.clone(),
            caller: self.routed_caller.clone().or_else(|| {
                (!self.context.original_caller.is_empty()).then(|| self.context.original_caller.clone())
            }),
            callee: self.connected_callee.clone().or_else(|| self.routed_callee.clone()).or_else(|| {
                (!self.context.original_callee.is_empty()).then(|| self.context.original_callee.clone())
            }),
            direction: self.context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };
        let leg = {
            let mut gw = gateway.write().await;
            if let Some(existing) = gw.get_leg(&call_id) {
                existing
            } else {
                let leg = RwiCallLeg::new_attached(&entry, handle);
                gw.register_leg(call_id.clone(), leg.clone());
                leg
            }
        };
        let ssrc = self.caller_leg.answered_ssrc();
        leg.set_peer(self.caller_leg.peer.clone()).await;
        leg.set_offer(self.caller_leg.offer_sdp.clone()).await;
        if let Some(answer_sdp) = self.caller_leg.answer_sdp.clone() {
            leg.set_answer(answer_sdp).await;
        }
        leg.set_negotiated_media(self.caller_leg.negotiated_audio.clone(), ssrc)
            .await;
        leg.set_state(if self.bridge_runtime.media_bridge.is_some() {
            RwiCallLegState::Bridged
        } else if self.caller_leg.negotiated_audio.is_some() {
            RwiCallLegState::Answered
        } else if self.ring_time.is_some() {
            RwiCallLegState::Ringing
        } else {
            RwiCallLegState::Initializing
        })
        .await;
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
            callee_dialogs: self.callee_leg.sip.recorded_dialogs(),
            server_dialog_id: self.server_dialog.id(),
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
        let caller_offer_sdp = self.caller_leg.offer_sdp.clone();
        self.caller_leg
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
        let caller_offer_sdp = self.caller_leg.offer_sdp.clone();
        self.caller_leg
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
            self.caller_leg.peer.clone(),
            self.callee_leg.peer.clone(),
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
            self.caller_leg
                .bridge_audio_selection(&self.context.dialplan.allow_codecs)
        })
        .unwrap_or(default_codec.clone());

        let caller_selection = if match_caller_to_callee_codec {
            self.caller_leg
                .bridge_audio_matching(Some(callee_selection.codec))
                .unwrap_or(default_codec)
        } else {
            self.caller_leg
                .bridge_audio_selection(&self.context.dialplan.allow_codecs)
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
<<<<<<< HEAD
    async fn create_offer_for_target(&mut self, target: &Location) -> Option<Vec<u8>> {
        debug!(
=======
    pub(crate) async fn create_offer_for_target(&mut self, target: &Location) -> Option<Vec<u8>> {
        info!(
>>>>>>> c8b7077 (refactore session.rs)
            session_id = %self.context.session_id,
            target = %target.aor,
            supports_webrtc = target.supports_webrtc,
            destination = ?target.destination,
            "create_offer_for_target called"
        );

        if !self.use_media_proxy {
            // Media proxy disabled: use caller's original offer
            return match self.callee_leg.offer_sdp.as_ref() {
                Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                _ => None,
            };
        }

        // Media proxy enabled: generate SDP based on target's WebRTC support
        match self.create_callee_track(target.supports_webrtc).await {
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
                match self.callee_leg.offer_sdp.as_ref() {
                    Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                    _ => None,
                }
            }
        }
    }

    pub async fn create_callee_track(&mut self, is_webrtc: bool) -> Result<String> {
<<<<<<< HEAD
        debug!(
            session_id = %self.context.session_id,
            is_webrtc,
            "create_callee_track called"
        );

        self.callee_leg.peer
            .remove_track(Self::RINGBACK_TRACK_ID, true)
            .await;
        let track_id = Self::CALLEE_TRACK_ID.to_string();
        let caller_rtp_map = self
            .caller_leg
            .offer_sdp
            .as_ref()
            .and_then(|caller_offer| {
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer).ok()
            })
            .and_then(|sdp| {
                sdp.media_sections
                    .iter()
                    .find(|m| m.kind == rustrtc::MediaKind::Audio)
                    .map(MediaNegotiator::parse_rtp_map_from_section)
            })
            .unwrap_or_default();

        let allow_codecs = &self.context.dialplan.allow_codecs;
        let mut codec_info = Vec::new();
        let mut seen_codecs = Vec::new();
        let mut used_payload_types = std::collections::HashSet::new();
        let caller_dtmf_codecs = Self::extract_telephone_event_codecs(&caller_rtp_map);

        for (pt, (codec, clock, channels)) in &caller_rtp_map {
            if *codec == CodecType::TelephoneEvent {
                continue;
            }
            if (!allow_codecs.is_empty() && !allow_codecs.contains(codec))
                || seen_codecs.contains(codec)
            {
                continue;
            }
            seen_codecs.push(*codec);
            used_payload_types.insert(*pt);
            codec_info.push(CodecInfo {
                payload_type: *pt,
                codec: *codec,
                clock_rate: *clock,
                channels: *channels,
            });
        }

        if !allow_codecs.is_empty() {
            let mut next_pt = 96;
            for codec in allow_codecs {
                if *codec == CodecType::TelephoneEvent {
                    continue;
                }
                if seen_codecs.contains(codec) {
                    continue;
                }
                seen_codecs.push(*codec);

                let payload_type = if !codec.is_dynamic() {
                    codec.payload_type()
                } else {
                    while used_payload_types.contains(&next_pt) {
                        next_pt += 1;
                    }
                    next_pt
                };
                used_payload_types.insert(payload_type);
                codec_info.push(CodecInfo {
                    payload_type,
                    codec: *codec,
                    clock_rate: codec.clock_rate(),
                    channels: codec.channels(),
                });
            }
        }

        Self::append_supported_dtmf_codecs(
            &mut codec_info,
            &caller_dtmf_codecs,
            &mut used_payload_types,
        );

        // Unified track creation using RtpTrackBuilder
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.callee_leg.peer.cancel_token())
            .with_codec_info(codec_info)
            .with_enable_latching(self.context.media_config.enable_latching);

        if let Some(ref addr) = self.context.media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }

        let (start_port, end_port) = if is_webrtc {
            (
                self.context.media_config.webrtc_port_start,
                self.context.media_config.webrtc_port_end,
            )
        } else {
            (
                self.context.media_config.rtp_start_port,
                self.context.media_config.rtp_end_port,
            )
        };

        if let (Some(start), Some(end)) = (start_port, end_port) {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        // Set mode based on is_webrtc flag
        if is_webrtc {
            debug!(
                session_id = %self.context.session_id,
                "Setting callee track to WebRTC mode"
            );
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        } else {
            debug!(
                session_id = %self.context.session_id,
                "Using default RTP mode for callee track"
            );
        }

        if let Some(existing) = self.find_track(&self.callee_leg.peer, &track_id).await {
            info!(
                session_id = %self.context.session_id,
                %track_id,
                "Reusing existing callee track for local description"
            );
            debug!(%track_id, "Reusing existing callee track for local description");
            let guard = existing.lock().await;
            guard.local_description().await
        } else {
            debug!(
                session_id = %self.context.session_id,
                %track_id,
=======
        self.callee_leg
            .media
            .create_local_track_offer(
                Self::CALLEE_TRACK_ID.to_string(),
>>>>>>> c8b7077 (refactore session.rs)
                is_webrtc,
                self.caller_leg.offer_sdp.as_deref(),
                &self.context.dialplan.allow_codecs,
                &self.context.media_config,
            )
            .await
    }

    pub(crate) async fn setup_callee_track(
        &mut self,
        callee_answer_sdp: &String,
        _dialog_id: Option<&DialogId>,
    ) -> Result<()> {
        self.callee_leg
            .media
            .apply_remote_answer(
                Self::CALLEE_TRACK_ID,
                callee_answer_sdp,
                &self.context.media_config,
            )
            .await?;
        self.callee_leg.media.select_or_store_negotiated_audio(
            MediaEndpoint::select_best_audio_from_sdp(
                callee_answer_sdp,
                &self.context.dialplan.allow_codecs,
            ),
        );
        Ok(())
    }

    pub fn add_callee_dialog(&mut self, dialog_id: DialogId) {
        {
            let callee_dialogs = self.callee_leg.sip.active_dialog_ids.lock().unwrap();
            if callee_dialogs.contains(&dialog_id) {
                return;
            }
        }
        self.callee_leg.sip.add_dialog(dialog_id.clone());
        if let Some(handle) = &self.handle {
            self.shared
                .register_dialog(dialog_id.to_string(), handle.clone());
        }
    }

    pub async fn start_ringing(&mut self, answer: String) {
        AnswerRuntime::start_ringing(self, answer).await;
    }

    pub async fn handle_reinvite(&mut self, method: rsip::Method, sdp: Option<String>) {
        if self.caller_leg.answer_sdp.is_none() {
            self.caller_leg.answer_sdp = self.shared.answer_sdp();
        }
        info!(session_id = %self.context.session_id, ?method, has_answer = self.caller_leg.answer_sdp.is_some(), "Handle re-INVITE/UPDATE");

        if let Some(offer) = sdp {
            debug!(?method, "Received Re-invite/UPDATE with SDP (Offer)");
            self.negotiation_state = NegotiationState::RemoteOfferReceived;

            if let Err(e) = self.caller_leg.media.update_remote_offer("caller-track", &offer).await {
                warn!(?method, "Failed to update caller peer with offer: {}", e);
                let _ = self
                    .server_dialog
                    .reject(Some(StatusCode::NotAcceptableHere), None);
                self.negotiation_state = NegotiationState::Stable;
                return;
            }

            self.callee_leg
                .sip
                .forward_remote_offer_to_client_dialog(
                    self.dialog_layer.clone(),
                    method.clone(),
                    offer.clone(),
                )
                .await;

            if method == rsip::Method::Invite {
                if self
                    .caller_leg
                    .sip
                    .reply_to_server_reinvite(&self.server_dialog, self.caller_leg.answer_sdp.clone())
                {
                    info!(?method, "Replied to re-INVITE with current SDP");
                }
                self.negotiation_state = NegotiationState::Stable;
            } else {
                self.negotiation_state = NegotiationState::Stable;
                debug!(?method, "Marked negotiation stable after processing UPDATE");
            }
        } else {
            debug!(
                ?method,
                "Received Re-invite/UPDATE without SDP (Request for Offer)"
            );
            self.negotiation_state = NegotiationState::LocalOfferSent;

            self.caller_leg.sip.reply_to_server_offerless_update(
                &self.server_dialog,
                self.caller_leg.answer_sdp.clone(),
            );
            self.negotiation_state = NegotiationState::Stable;
        }
    }

    pub async fn handle_trickle_ice(&mut self, payload: &str) {
        for line in payload.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let candidate = line
                .strip_prefix("a=candidate:")
                .or_else(|| line.strip_prefix("candidate:"));
            if let Some(candidate) = candidate {
                if let Err(err) = self
                    .caller_leg
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
        self.caller_leg.answer_sdp = Some(sdp);
    }

    pub(crate) fn freeze_answered_caller_audio(&mut self) {
        self.caller_leg
            .media
            .freeze_answered_audio_with_allow_codecs(&self.context.dialplan.allow_codecs);
    }

    pub async fn accept_call(
        &mut self,
        callee: Option<String>,
        callee_answer: Option<String>,
        dialog_id: Option<String>,
    ) -> Result<()> {
<<<<<<< HEAD
        // Ensure queue hold tones cease immediately once the call is answered.
        // self.stop_queue_hold().await;

        let first_answer = self.answer_time.is_none();

        if let Some(callee_addr) = callee {
            let resolved_callee = self.routed_callee.clone().unwrap_or(callee_addr);
            self.connected_callee = Some(resolved_callee);
        }
        if let Some(ref id_str) = dialog_id {
            let matched_dialog_id = self
                .callee_leg
                .dialog_ids
                .lock()
                .unwrap()
                .iter()
                .find(|id| id.to_string() == **id_str)
                .cloned();
            if let Some(id) = matched_dialog_id {
                self.callee_leg.connected_dialog_id = Some(id.clone());
            }
        }
        if first_answer {
            self.answer_time = Some(Instant::now());
        }
        debug!(
            server_dialog_id = %self.server_dialog.id(),
            use_media_proxy = self.use_media_proxy,
            has_answer = self.caller_leg.answer_sdp.is_some(),
            dialog_id = ?dialog_id,
            "Call answered"
        );

        // Only optimize the caller-facing answer before we have sent the initial 200 OK.
        // Once the inbound leg is already answered, changing local preferences here does not
        // renegotiate the caller endpoint and can leave the bridge assuming the wrong codec.
        if first_answer && !self.caller_leg.early_media_sent {
            if let Some(ref callee_sdp) = callee_answer {
                if let Some(optimized_answer) = self.optimize_caller_codec(callee_sdp).await {
                    self.set_answer(optimized_answer);
                }
            }
        }

        if self.caller_leg.answer_sdp.is_none() {
            let answer_for_caller = self.create_caller_answer_from_offer().await?;
            self.set_answer(answer_for_caller);
        }

        if first_answer {
            self.freeze_answered_caller_audio();
        }

        if self.use_media_proxy {
            let track_id = Self::CALLEE_TRACK_ID.to_string();
            self.caller_leg.peer
                .remove_track(Self::RINGBACK_TRACK_ID, true)
                .await;
            if let Some(answer) = callee_answer.as_ref() {
                self.setup_callee_track(answer, None).await?;

                if self.media_bridge.is_none() {
                    if let Some(cancel) = self.caller_leg.dtmf_listener_cancel.take() {
                        cancel.cancel();
                        self.shared.set_dtmf_listener_cancel(None);
                    }
                    let default_codec = Self::explicit_audio_default_selection();
                    let (codec_b, params_b, dtmf_pt_b) = self
                        .select_best_audio_from_sdp(answer)
                        .or_else(|| {
                            self.caller_leg
                                .answer_sdp
                                .as_deref()
                                .and_then(|s| self.select_best_audio_from_sdp(s))
                        })
                        .or_else(|| {
                            self.caller_leg
                                .offer_sdp
                                .as_deref()
                                .and_then(|s| self.select_best_audio_from_sdp(s))
                        })
                        .unwrap_or(default_codec.clone());
                    let (codec_a, params_a, dtmf_pt_a) = self
                        .caller_leg
                        .negotiated_audio
                        .clone()
                        .or_else(|| {
                            self.caller_leg
                                .answer_sdp
                                .as_deref()
                                .and_then(|s| self.select_best_audio_from_sdp(s))
                        })
                        .or_else(|| {
                            self.caller_leg
                                .offer_sdp
                                .as_deref()
                                .and_then(|s| self.select_best_audio_from_sdp(s))
                        })
                        .unwrap_or(default_codec);

                    let ssrc_a = self
                        .caller_leg
                        .answer_sdp
                        .as_ref()
                        .and_then(|s| MediaNegotiator::extract_ssrc(s));
                    let ssrc_b = MediaNegotiator::extract_ssrc(answer);

                    debug!(
                        ?codec_a,
                        ?params_a,
                        ?codec_b,
                        ?params_b,
                        ssrc_a,
                        ssrc_b,
                        "Media bridge for call session"
                    );

                    let bridge = MediaBridge::new(
                        self.caller_leg.peer.clone(),
                        self.callee_leg.peer.clone(),
                        params_a,
                        params_b,
                        dtmf_pt_a,
                        dtmf_pt_b,
                        codec_a,
                        codec_b,
                        ssrc_a,
                        ssrc_b,
                        self.recorder_option.clone(),
                        self.context
                            .dialplan
                            .call_id
                            .clone()
                            .unwrap_or_else(|| self.context.session_id.clone()),
                        self.server.sip_flow.as_ref().and_then(|sf| sf.backend()),
                    );
                    self.media_bridge = Some(bridge);
                }
            }

            if let Some(ref bridge) = self.media_bridge {
                if let Err(e) = bridge.start().await {
                    warn!(session_id = %self.context.session_id, "Failed to start media bridge: {}", e);
                }
                let _ = bridge.resume_forwarding(&track_id).await;
            }
        } else {
            if let Some(answer) = callee_answer {
                self.set_answer(answer);
            }
            if let Some(ref bridge) = self.media_bridge {
                bridge.stop();
            }
            self.caller_leg.peer.stop();
            self.callee_leg.peer.stop();
        }

        let mut headers = if self.caller_leg.answer_sdp.is_some() {
            vec![rsip::Header::ContentType("application/sdp".into())]
        } else {
            vec![]
        };

        {
            let server_timer = self.server_timer.lock().unwrap();
            if server_timer.active {
                headers.push(rsip::Header::Supported(
                    rsip::headers::Supported::from(TIMER_TAG).into(),
                ));
                headers.push(rsip::Header::Other(
                    HEADER_SESSION_EXPIRES.into(),
                    format!(
                        "{};refresher={}",
                        server_timer.session_interval.as_secs(),
                        server_timer.refresher
                    ),
                ));
            }
        }

        if let Err(e) = self.server_dialog.accept(
            Some(headers),
            self.caller_leg.answer_sdp.clone().map(|sdp| sdp.into_bytes()),
        ) {
            return Err(anyhow!("Failed to send 200 OK: {}", e));
        }
        self.mark_active_call_answered();
        if first_answer {
            let callee = self
                .connected_callee
                .clone()
                .or_else(|| self.routed_callee.clone());
            self.shared
                .emit_custom_event(ProxyCallEvent::TargetAnswered {
                    session_id: self.shared.session_id(),
                    callee,
                });
        }
        self.sync_rwi_attached_leg().await;
        Ok(())
=======
        AnswerRuntime::accept_call(self, callee, callee_answer, dialog_id).await
>>>>>>> c8b7077 (refactore session.rs)
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
            callee_dialogs: vec![],
            server_dialog_id,
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
                        .caller_leg
                        .sip
                        .hangup_inbound_dialog(&self.server_dialog, Some(status), reason_str.clone())
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
                    if let Some(endpoint) = TransferEndpoint::parse(&target) {
                        self.transfer_to_endpoint(&endpoint, inbox).await
                    } else {
                        self.transfer_to_uri(&target).await
                    }
                }
                SessionAction::HandleReInvite(method_str, sdp) => {
                    let sdp_opt = if sdp.is_empty() { None } else { Some(sdp) };
                    let method = match method_str.to_uppercase().as_str() {
                        "INVITE" => rsip::Method::Invite,
                        "UPDATE" => rsip::Method::Update,
                        _ => rsip::Method::Invite, // Default to Invite for now
                    };
                    self.handle_reinvite(method, sdp_opt).await;
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
                    playback_runtime::play_audio_file(
                        &self.caller_leg.media,
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
                    recording_runtime::start_recording(
                        &mut self.caller_leg.media,
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
                    recording_runtime::stop_recording(
                        &mut self.caller_leg.media,
                        &self.context.session_id,
                        &mut self.recording_state,
                        self.app_event_tx.clone(),
                    )
                    .await?;
                    Ok(())
                }
                SessionAction::StopPlayback => {
                    self.caller_leg.media.remove_track("prompt").await;
                    Ok(())
                }
                SessionAction::Hold { music_source } => {
                    let audio_file = music_source.clone().unwrap_or_default();
                    if !audio_file.is_empty() {
                        playback_runtime::play_audio_file(
                            &self.caller_leg.media,
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
                SessionAction::Unhold => {
                    self.caller_leg.media.remove_track("hold_music").await;
                    Ok(())
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
                            self.caller_leg.peer.clone(),
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
                            self.caller_leg.peer.clone(),
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
                            self.caller_leg.peer.clone(),
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
        let invite_option = self.callee_leg.sip.build_outbound_invite_option(
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

<<<<<<< HEAD
    async fn execute_invite(
        &mut self,
        mut invite_option: InviteOption,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let _state_tx_keepalive = state_tx.clone();
        let dialog_layer = self.dialog_layer.clone();
        self.note_invite_details(&invite_option);

        // Log client INVITE details
        debug!(
            session_id = %self.context.session_id,
            callee = %invite_option.callee,
            destination = ?invite_option.destination,
            "Sending client INVITE to target"
        );

        let session_id = self.context.session_id.clone();
        let mut retry_count = 0;
        let mut invitation = dialog_layer
            .do_invite(invite_option.clone(), state_tx.clone())
            .boxed();
        let dialog;
        let response;
        loop {
            tokio::select! {
                res = &mut invitation => {
                    match res {
                        Ok((dialog_id, resp)) => {
                            if let Some(resp) = resp {
                                if resp.status_code == StatusCode::SessionIntervalTooSmall {
                                    if retry_count < 1 {
                                        let min_se_value =
                                            get_header_value(&resp.headers, HEADER_MIN_SE);
                                        if let Some(value) = min_se_value {
                                            if let Some(min_se) = parse_min_se(&value) {
                                                info!(
                                                    session_id = %session_id,
                                                    min_se = ?min_se,
                                                    "Received 422, retrying with new Session-Expires"
                                                );
                                                if let Some(headers) =
                                                    &mut invite_option.headers
                                                {
                                                    headers.retain(|h| match h {
                                                        rsip::Header::Other(n, _) => !n
                                                            .eq_ignore_ascii_case(
                                                                HEADER_SESSION_EXPIRES,
                                                            ),
                                                        _ => true,
                                                    });
                                                    headers.push(rsip::Header::Other(
                                                        HEADER_SESSION_EXPIRES.into(),
                                                        min_se.as_secs().to_string(),
                                                    ));
                                                }
                                                retry_count += 1;
                                                invitation = dialog_layer.do_invite(invite_option.clone(), state_tx.clone()).boxed();
                                                continue;
                                            }
                                        }
                                    }
                                }

                                if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    return Err((resp.status_code, reason));
                                } else {
                                    dialog = dialog_id;
                                    response = resp;
                                    break;
                                }
                            } else {
                                return Err((
                                    StatusCode::RequestTerminated,
                                    Some("Cancelled by callee".to_string()),
                                ));
                            }
                        }
                        Err(e) => {
                            debug!(session_id = %session_id, "Invite failed: {:?}", e);
                            return Err(match e {
                                rsipstack::Error::DialogError(reason, _, code) => (code, Some(reason)),
                                _ => (
                                    StatusCode::ServerInternalError,
                                    Some("Invite failed".to_string()),
                                ),
                            });
                        }
                    }
                }
                state = state_rx.recv() => {
                    match state {
                        Some(DialogState::Early(_, response)) => {
                            let sdp = String::from_utf8_lossy(response.body()).to_string();
                            let action = SessionAction::StartRinging{
                                ringback: Some(sdp),
                                passthrough: false
                            };
                            self.apply_session_action(action, None).await.ok();
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut state_rx_guard = DialogStateReceiverGuard::new(dialog_layer.clone(), state_rx);
        state_rx_guard.set_dialog_id(dialog.id());

        if self.server.proxy_config.session_timer {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            self.init_client_timer(&response, default_expires);
        }

        if response.status_code.kind() == rsip::StatusCodeKind::Successful {
            // Extract SDP from response body, if present
            let answer_body = response.body();
            let sdp = if !answer_body.is_empty() {
                Some(String::from_utf8_lossy(answer_body).to_string())
            } else {
                None
            };

            let dialog_id_val = dialog.id();

            // Log client dialog details after successful answer
            debug!(
                session_id = %self.context.session_id,
                dialog_id = %dialog_id_val,
                "Client dialog established after 200 OK from callee"
            );

            self.add_callee_dialog(dialog_id_val.clone());
            let _ = self
                .apply_session_action(
                    SessionAction::AcceptCall {
                        callee: Some(target.aor.to_string()),
                        sdp,
                        dialog_id: Some(dialog_id_val.to_string()),
                    },
                    None,
                )
                .await;
        }

        self.add_callee_guard(state_rx_guard);
        Ok(())
    }

    async fn handle_failure(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        if self.failure_is_busy() {
            if let Some(config) = self.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenBusy) {
                    info!(
                        session_id = %self.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (busy) engaged"
                    );
                    return self.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
            // Voicemail on busy (if no explicit busy forwarding configured)
            if self.context.dialplan.voicemail_enabled {
                info!(
                    session_id = %self.context.session_id,
                    callee = %self.context.original_callee,
                    "busy: routing to voicemail"
                );
                let params = serde_json::json!({
                    "extension": self.context.original_callee,
                    "caller_id": self.context.original_caller,
                });
                return self.run_application("voicemail", Some(params), true).await;
            }
        } else if self.failure_is_no_answer() {
            if let Some(config) = self.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenNoAnswer) {
                    info!(
                        session_id = %self.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (no answer) engaged"
                    );
                    return self.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
            // Voicemail on no-answer (if no explicit no-answer forwarding configured)
            if self.context.dialplan.voicemail_enabled {
                info!(
                    session_id = %self.context.session_id,
                    callee = %self.context.original_callee,
                    "no-answer: routing to voicemail"
                );
                let params = serde_json::json!({
                    "extension": self.context.original_callee,
                    "caller_id": self.context.original_caller,
                });
                return self.run_application("voicemail", Some(params), true).await;
            }
        }

        if let Some((code, reason)) = self.last_error.clone() {
            self.report_failure(code, reason)?;
        } else {
            self.report_failure(
                StatusCode::ServerInternalError,
                Some("Unknown error".to_string()),
            )?;
        }
        Err(anyhow!("Call failed"))
=======
    pub(crate) async fn handle_failure(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        TargetRuntime::handle_failure(self, inbox).await
>>>>>>> c8b7077 (refactore session.rs)
    }

    /// Run a call application (voicemail, IVR, etc.) by looking up the addon,
    /// instantiating the app, and driving it through `AppEventLoop`.
    pub(crate) async fn run_application(
        &mut self,
        app_name: &str,
        app_params: Option<serde_json::Value>,
        auto_answer: bool,
    ) -> Result<()> {
        let app = AppRuntime::build_call_app(self, app_name, &app_params).await?;
        AppRuntime::run(self, app_name, app, auto_answer).await
    }


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

        let caller_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", context.session_id))
            .with_cancel_token(cancel_token.child_token());
        let caller_peer = Arc::new(VoiceEnginePeer::new(Arc::new(caller_media_builder.build())));

        let callee_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", context.session_id))
            .with_cancel_token(cancel_token.child_token());
        let callee_peer = Arc::new(VoiceEnginePeer::new(Arc::new(callee_media_builder.build())));

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
            server_dialog.clone(),
            use_media_proxy,
            recorder_option,
            caller_peer,
            callee_peer,
            session_shared.clone(),
            Some(reporter),
        ));

        session.caller_leg.sip.supports_trickle_ice =
            SipLeg::has_trickle_ice_support(&initial_request.headers);

        if use_media_proxy {
            session.caller_leg.offer_sdp = Some(offer_sdp);
            session.callee_leg.offer_sdp =
                session.create_callee_track(all_webrtc_target).await.ok();
        } else {
            session.caller_leg.offer_sdp = Some(offer_sdp.clone());
            session.callee_leg.offer_sdp = Some(offer_sdp);
        }

        let dialog_guard =
            ServerDialogGuard::new(server.dialog_layer.clone(), session.server_dialog.id());
        let (handle, action_rx) = CallSessionHandle::with_shared(session_shared.clone());
        session.register_active_call(handle);

        let (callee_state_tx, callee_state_rx) = mpsc::unbounded_channel();
        session.callee_leg.sip.dialog_event_tx = Some(callee_state_tx);

        let action_inbox = SessionActionInbox::new(action_rx);

        let mut server_dialog_clone = session.server_dialog.clone();
        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, action_inbox, dialog_guard)
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

    pub async fn process(
        mut self: Box<Self>,
        state_rx: mpsc::UnboundedReceiver<DialogState>,
        callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut action_inbox: SessionActionInbox,
        dialog_guard: ServerDialogGuard,
    ) {
        let _cancel_token_guard = self.cancel_token.clone().drop_guard();

        let use_media_proxy = self.use_media_proxy;
        let caller_peer = self.caller_leg.peer.clone();
        let callee_peer = self.callee_leg.peer.clone();

        let _guard = dialog_guard;

        if self.server.proxy_config.session_timer {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            if let Err((code, _min_se)) = self.init_server_timer(default_expires) {
                info!("Rejecting call with 422 Session Interval Too Small");
                let _ = self.server_dialog.reject(Some(code), None);
                return;
            }
        }

        let server_timer = self.caller_leg.sip.session_timer.clone();
        let client_timer = self.callee_leg.sip.session_timer.clone();
        let callee_dialogs = self.callee_leg.sip.active_dialog_ids.clone();
        let server_dialog_clone = self.server_dialog.clone();
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
                if let Some(reason) = hangup_reason.clone() {
                    self.hangup_reason = Some(reason.clone());
                    self.shared.mark_hangup(reason);
                } else {
                    self.hangup_reason = Some(CallRecordHangupReason::Canceled);
                    self.shared
                        .mark_hangup(CallRecordHangupReason::Canceled);
                }
                Err(anyhow!("Call cancelled"))
            }
            _ = async {
                if use_media_proxy {
                    let _ = tokio::join!(caller_peer.serve(), callee_peer.serve());
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
<<<<<<< HEAD
                self.execute_dialplan(Some(&mut action_inbox)).await?;
                debug!(session_id = %self.context.session_id, "Dialplan execution finished, waiting for call termination");
=======
                DialplanRuntime::execute_dialplan(&mut self, Some(&mut action_inbox)).await?;
                info!(session_id = %self.context.session_id, "Dialplan execution finished, waiting for call termination");
>>>>>>> c8b7077 (refactore session.rs)
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
                callee_state_rx,
                server_timer,
                client_timer,
                callee_dialogs.clone(),
                server_dialog_clone.clone(),
                handle_for_events,
                cancel_token_for_loop,
                pending_hangup,
                shared_for_loop
            ) => r,
        };

        // Process pending hangup if any
        let (_, _, hangup_reason) = self.resolve_pending_hangup();
        if let Some(reason) = hangup_reason {
            self.hangup_reason = Some(reason.clone());
            self.shared.mark_hangup(reason);
        }

        // Terminate all client dialogs
        {
            let dialogs = callee_dialogs.lock().unwrap().clone();
            for dialog_id in dialogs {
                if let Some(dialog) = dialog_layer_clone.get_dialog(&dialog_id) {
                    debug!(session_id = %context_clone.session_id, %dialog_id, "Terminating client dialog");
                    dialog_layer_clone.remove_dialog(&dialog_id);
                    dialog.hangup().await.ok();
                }
            }
        }

        if !self.server_dialog.state().is_terminated() {
            let server_state = self.server_dialog.state();
            let normal_hangup = matches!(
                self.hangup_reason,
                Some(CallRecordHangupReason::ByCallee) | Some(CallRecordHangupReason::ByCaller)
            );

            if normal_hangup && (server_state.is_confirmed() || server_state.waiting_ack()) {
                info!(
                    session_id = %self.context.session_id,
                    hangup_reason = ?self.hangup_reason,
                    "Sending BYE to caller for normal call teardown"
                );
                let _ = self.server_dialog.bye().await;
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
                let _ = self.server_dialog.reject(Some(code), reason);
            }
        }
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
        self.caller_leg.peer.stop();
        self.callee_leg.peer.stop();
        if let Some(reporter) = self.reporter.take() {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
        }
    }
}

// ─────────────────────────────────────────────────────────────
// DTMF reception helpers (RFC 4733 / RTP telephone-event)
// ─────────────────────────────────────────────────────────────
