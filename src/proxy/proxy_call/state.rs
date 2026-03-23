use crate::call::{DialDirection, Dialplan, MediaConfig, TransactionCookie};
use crate::callrecord::CallRecordHangupReason;
use crate::media::negotiate::CodecInfo;
use crate::proxy::active_call_registry::{
    ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
};
use crate::proxy::proxy_call::media_peer::MediaPeer;
use crate::rwi::SupervisorMode;
use crate::rwi::proto::RwiEvent;
use anyhow::Result;
use audio_codec::CodecType;
use chrono::{DateTime, Utc};
use rsip::StatusCode;
use rsipstack::dialog::dialog::TransactionHandle;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The kind of session driving this call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Normal B2BUA proxied call (inbound INVITE -> outbound INVITE).
    Proxy,
    /// RWI-originated single-leg call (no inbound server dialog).
    RwiSingleLeg,
    /// App-driven session (IVR, voicemail -- single leg + app).
    AppDriven,
}

/// Immutable context for the entire duration of a call
#[derive(Clone)]
pub struct CallContext {
    pub session_id: String,
    pub kind: SessionKind,
    pub dialplan: Arc<Dialplan>,
    pub cookie: TransactionCookie,
    pub start_time: Instant,
    pub media_config: MediaConfig,
    pub original_caller: String,
    pub original_callee: String,
    pub max_forwards: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidDialogLeg {
    Caller,
    Callee,
}

impl MidDialogLeg {
    fn pending_key(&self, dialog_id: &str) -> String {
        let leg = match self {
            Self::Caller => "caller",
            Self::Callee => "callee",
        };
        format!("{leg}:{dialog_id}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionAction {
    AcceptCall {
        callee: Option<String>,
        sdp: Option<String>,
        dialog_id: Option<String>,
    },
    TransferTarget(String),
    ProvideEarlyMedia(String),
    ProvideEarlyMediaWithDialog(String, String),
    StartRinging {
        ringback: Option<String>,
        passthrough: bool,
    },
    PlayPrompt {
        audio_file: String,
        send_progress: bool,
        await_completion: bool,
        #[serde(default)]
        track_id: Option<String>,
        #[serde(default)]
        loop_playback: bool,
        #[serde(default)]
        interrupt_on_dtmf: bool,
    },
    StartRecording {
        path: String,
        max_duration: Option<std::time::Duration>,
        beep: bool,
    },
    PauseRecording,
    ResumeRecording,
    StopRecording,
    Hangup {
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    HandleReInvite {
        leg: MidDialogLeg,
        method: String,
        sdp: Option<String>,
        dialog_id: String,
    },
    HandleTrickleIce(String),
    RefreshSession,
    MuteTrack(String),
    UnmuteTrack(String),
    BridgeTo {
        target_session_id: String,
    },
    Unbridge,
    StopPlayback,
    SupervisorListen {
        target_session_id: String,
    },
    SupervisorWhisper {
        target_session_id: String,
    },
    SupervisorBarge {
        target_session_id: String,
    },
    SupervisorStop,
    StartSupervisorMode {
        supervisor_session_id: String,
        target_session_id: String,
        mode: SupervisorMode,
    },
    Hold {
        music_source: Option<String>,
    },
    Unhold,
}

impl SessionAction {
    pub fn from_transfer_target(target: &str) -> Self {
        let trimmed = target.trim();
        Self::TransferTarget(trimmed.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyCallPhase {
    Initializing,
    Routing,
    App,
    Dialing,
    Ringing,
    EarlyMedia,
    Bridged,
    Terminating,
    Failed,
    Ended,
}

impl ProxyCallPhase {
    /// Returns `true` if the transition from the current phase to `target` is valid.
    pub fn can_transition_to(self, target: ProxyCallPhase) -> bool {
        use ProxyCallPhase::*;
        matches!(
            (self, target),
            // From Initializing
            (Initializing, Routing)
            | (Initializing, App)
            | (Initializing, Dialing)
            | (Initializing, Ringing)
            | (Initializing, EarlyMedia)
            | (Initializing, Bridged)
            | (Initializing, Terminating)
            | (Initializing, Failed)
            | (Initializing, Ended)
            // From Routing
            | (Routing, App)
            | (Routing, Dialing)
            | (Routing, Ringing)
            | (Routing, Terminating)
            | (Routing, Failed)
            // From App
            | (App, Dialing)
            | (App, Routing)
            | (App, Terminating)
            | (App, Failed)
            | (App, Ended)
            // From Dialing
            | (Dialing, Ringing)
            | (Dialing, EarlyMedia)
            | (Dialing, Bridged)
            | (Dialing, Terminating)
            | (Dialing, Failed)
            // From Ringing
            | (Ringing, EarlyMedia)
            | (Ringing, Bridged)
            | (Ringing, Terminating)
            | (Ringing, Failed)
            | (Ringing, Ended)
            // From EarlyMedia
            | (EarlyMedia, Bridged)
            | (EarlyMedia, Terminating)
            | (EarlyMedia, Failed)
            | (EarlyMedia, Ended)
            // From Bridged
            | (Bridged, Terminating)
            | (Bridged, Failed)
            | (Bridged, Ended)
            // From Terminating
            | (Terminating, Ended)
            | (Terminating, Failed)
            // From Failed
            | (Failed, Terminating)
            | (Failed, Ended)
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ProxyCallPhase::Ended)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CallSessionSnapshot {
    session_id: String,
    phase: ProxyCallPhase,
    started_at: DateTime<Utc>,
    ring_time: Option<DateTime<Utc>>,
    answer_time: Option<DateTime<Utc>>,
    hangup_reason: Option<CallRecordHangupReason>,
    last_error_code: Option<u16>,
    last_error_reason: Option<String>,
    caller: Option<String>,
    callee: Option<String>,
    current_target: Option<String>,
    queue_name: Option<String>,
    direction: String,
    pub answer_sdp: Option<String>,
}

/// Shared media state for the session's exported control/view leg.
///
/// Updated by `CallSession` when exported leg media state changes;
/// read by `RwiCallLeg` to avoid manual sync and duplicate ownership.
#[derive(Default)]
pub struct SharedExportedLegMedia {
    pub peer: Option<Arc<dyn MediaPeer>>,
    pub offer_sdp: Option<String>,
    pub answer_sdp: Option<String>,
    pub negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    pub ssrc: Option<u32>,
    pub is_bridged: bool,
}

pub type SharedExportedLegMediaRef = Arc<RwLock<SharedExportedLegMedia>>;

#[derive(Clone)]
pub struct CallSessionShared {
    inner: Arc<RwLock<CallSessionSnapshot>>,
    registry: Option<Weak<ActiveProxyCallRegistry>>,
    events: Arc<RwLock<Option<ProxyCallEventSender>>>,
    app_event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>>>,
    dtmf_listener_cancel: Arc<RwLock<Option<CancellationToken>>>,
    pending_mid_dialog_replies: Arc<Mutex<HashMap<String, TransactionHandle>>>,
    shared_exported_leg_media: SharedExportedLegMediaRef,
}

impl CallSessionShared {
    pub fn new(
        session_id: String,
        direction: DialDirection,
        caller: Option<String>,
        callee: Option<String>,
        registry: Option<Arc<ActiveProxyCallRegistry>>,
    ) -> Self {
        let started_at = Utc::now();
        let inner = CallSessionSnapshot {
            session_id: session_id.clone(),
            phase: ProxyCallPhase::Initializing,
            started_at,
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            last_error_code: None,
            last_error_reason: None,
            caller,
            callee,
            current_target: None,
            queue_name: None,
            direction: direction.to_string(),
            answer_sdp: None,
        };
        Self {
            inner: Arc::new(RwLock::new(inner)),
            registry: registry.map(|r| Arc::downgrade(&r)),
            events: Arc::new(RwLock::new(None)),
            app_event_tx: Arc::new(RwLock::new(None)),
            dtmf_listener_cancel: Arc::new(RwLock::new(None)),
            pending_mid_dialog_replies: Arc::new(Mutex::new(HashMap::new())),
            shared_exported_leg_media: Arc::new(RwLock::new(SharedExportedLegMedia::default())),
        }
    }

    /// Set (or clear) the app-event sender used by [`send_app_event`].
    ///
    /// Called by `run_application` at the start and end of a call app.
    pub fn set_app_event_sender(
        &self,
        sender: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    ) {
        if let Ok(mut slot) = self.app_event_tx.write() {
            *slot = sender;
        }
    }

    /// Send a [`ControllerEvent`] directly to the running `CallApp` event loop,
    /// bypassing the `SessionAction` / `action_inbox` path.
    ///
    /// Returns `true` if the event was delivered (i.e. an app is currently running).
    pub fn send_app_event(&self, event: crate::call::app::ControllerEvent) -> bool {
        if let Ok(slot) = self.app_event_tx.read() {
            if let Some(tx) = slot.as_ref() {
                return tx.send(event).is_ok();
            }
        }
        false
    }

    pub fn set_dtmf_listener_cancel(&self, cancel: Option<CancellationToken>) {
        if let Ok(mut slot) = self.dtmf_listener_cancel.write() {
            *slot = cancel;
        }
    }

    pub fn cancel_dtmf_listener(&self) {
        let cancel = self
            .dtmf_listener_cancel
            .write()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    /// Get a reference to the shared exported leg media for RWI consumers.
    pub fn shared_exported_leg_media(&self) -> SharedExportedLegMediaRef {
        self.shared_exported_leg_media.clone()
    }

    /// Update the shared exported leg media from current leg state.
    pub fn publish_exported_leg_media(
        &self,
        peer: Arc<dyn MediaPeer>,
        offer_sdp: Option<String>,
        answer_sdp: Option<String>,
        negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
        ssrc: Option<u32>,
        is_bridged: bool,
    ) {
        if let Ok(mut media) = self.shared_exported_leg_media.write() {
            media.peer = Some(peer);
            media.offer_sdp = offer_sdp;
            media.answer_sdp = answer_sdp;
            media.negotiated_audio = negotiated_audio;
            media.ssrc = ssrc;
            media.is_bridged = is_bridged;
        }
    }

    pub fn store_mid_dialog_reply(
        &self,
        leg: MidDialogLeg,
        dialog_id: &str,
        handle: TransactionHandle,
    ) {
        if let Ok(mut replies) = self.pending_mid_dialog_replies.lock() {
            replies.insert(leg.pending_key(dialog_id), handle);
        }
    }

    pub fn take_mid_dialog_reply(
        &self,
        leg: MidDialogLeg,
        dialog_id: &str,
    ) -> Option<TransactionHandle> {
        self.pending_mid_dialog_replies
            .lock()
            .ok()
            .and_then(|mut replies| replies.remove(&leg.pending_key(dialog_id)))
    }

    pub fn snapshot(&self) -> CallSessionSnapshot {
        let inner = self.inner.read().unwrap();
        inner.clone()
    }

    pub fn set_answer_sdp(&self, sdp: String) {
        let mut inner = self.inner.write().unwrap();
        inner.answer_sdp = Some(sdp);
    }

    pub fn answer_sdp(&self) -> Option<String> {
        let inner = self.inner.read().unwrap();
        inner.answer_sdp.clone()
    }

    pub fn rwi_hangup_event(&self) -> RwiEvent {
        let inner = self.inner.read().unwrap();
        let reason = inner
            .hangup_reason
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| inner.last_error_reason.clone());
        let sip_status = inner
            .last_error_code
            .or_else(|| inner.hangup_reason.as_ref().map(|_| 200));
        RwiEvent::CallHangup {
            call_id: inner.session_id.clone(),
            reason,
            sip_status,
        }
    }

    pub fn queue_name(&self) -> Option<String> {
        let inner = self.inner.read().unwrap();
        inner.queue_name.clone()
    }

    pub fn register_active_call(&self, handle: CallSessionHandle) {
        if let Some(registry) = self.registry.as_ref().and_then(|r| r.upgrade()) {
            let inner = self.inner.read().unwrap();
            let entry = ActiveProxyCallEntry {
                session_id: inner.session_id.clone(),
                caller: inner.caller.clone(),
                callee: inner.callee.clone(),
                direction: inner.direction.clone(),
                started_at: inner.started_at,
                answered_at: inner.answer_time,
                status: ActiveProxyCallStatus::Ringing,
            };
            registry.upsert(entry, handle);
        }
    }

    pub fn register_dialog(&self, dialog_id: String, handle: CallSessionHandle) {
        if let Some(registry) = self.registry.as_ref().and_then(|r| r.upgrade()) {
            registry.register_dialog(dialog_id, handle);
        }
    }

    pub fn unregister(&self) {
        if let Some(registry) = self.registry.as_ref().and_then(|r| r.upgrade()) {
            let inner = self.inner.read().unwrap();
            registry.remove(&inner.session_id);
        }
    }

    pub fn set_event_sender(&self, sender: ProxyCallEventSender) {
        if let Ok(mut slot) = self.events.write() {
            *slot = Some(sender);
        }
    }

    pub fn emit_custom_event(&self, event: ProxyCallEvent) {
        if let Some(sender) = self.event_sender() {
            let _ = sender.send(event);
        }
    }

    pub fn session_id(&self) -> String {
        self.inner
            .read()
            .map(|inner| inner.session_id.clone())
            .unwrap_or_default()
    }

    pub fn update_routed_parties(&self, caller: Option<String>, callee: Option<String>) {
        self.update(|inner| {
            let mut changed = false;
            if let Some(ref caller) = caller {
                if inner.caller != Some(caller.clone()) {
                    inner.caller = Some(caller.clone());
                    changed = true;
                }
            }
            if let Some(ref callee) = callee {
                if inner.callee != Some(callee.clone()) {
                    inner.callee = Some(callee.clone());
                    changed = true;
                }
            }
            changed
        });
    }

    pub fn transition_to_routing(&self) -> bool {
        self.update(|inner| {
            if inner.phase.can_transition_to(ProxyCallPhase::Routing) {
                inner.phase = ProxyCallPhase::Routing;
                true
            } else {
                false
            }
        })
    }

    pub fn transition_to_app(&self) -> bool {
        self.update(|inner| {
            if inner.phase.can_transition_to(ProxyCallPhase::App) {
                inner.phase = ProxyCallPhase::App;
                true
            } else {
                false
            }
        })
    }

    pub fn transition_to_dialing(&self) -> bool {
        self.update(|inner| {
            if inner.phase.can_transition_to(ProxyCallPhase::Dialing) {
                inner.phase = ProxyCallPhase::Dialing;
                true
            } else {
                false
            }
        })
    }

    pub fn transition_to_ringing(&self, has_early_media: bool) -> bool {
        let changed = self.update(|inner| match inner.phase {
            ProxyCallPhase::Initializing
            | ProxyCallPhase::Routing
            | ProxyCallPhase::Dialing
            | ProxyCallPhase::Ringing
            | ProxyCallPhase::EarlyMedia => {
                if inner.ring_time.is_none() {
                    inner.ring_time = Some(Utc::now());
                }
                inner.phase = if has_early_media {
                    ProxyCallPhase::EarlyMedia
                } else {
                    ProxyCallPhase::Ringing
                };
                true
            }
            ProxyCallPhase::App
            | ProxyCallPhase::Bridged
            | ProxyCallPhase::Terminating
            | ProxyCallPhase::Failed
            | ProxyCallPhase::Ended => false,
        });
        changed
    }

    pub fn transition_to_answered(&self) {
        self.update(|inner| {
            if inner.answer_time.is_none() {
                inner.answer_time = Some(Utc::now());
            }
            inner.phase = ProxyCallPhase::Bridged;
            true
        });
    }

    pub fn note_failure(&self, code: StatusCode, reason: Option<String>) {
        self.update(|inner| {
            inner.last_error_code = Some(u16::from(code.clone()));
            inner.last_error_reason = reason.clone();
            if inner.phase != ProxyCallPhase::Bridged {
                inner.phase = ProxyCallPhase::Failed;
            }
            true
        });
    }

    pub fn transition_to_terminating(&self) -> bool {
        self.update(|inner| {
            if matches!(
                inner.phase,
                ProxyCallPhase::Ended | ProxyCallPhase::Terminating
            ) {
                return false;
            }
            inner.phase = ProxyCallPhase::Terminating;
            true
        })
    }

    pub fn mark_hangup(&self, reason: CallRecordHangupReason) {
        self.update(|inner| {
            inner.hangup_reason = Some(reason);
            // Ensure we transition through Terminating before Ended
            if !matches!(
                inner.phase,
                ProxyCallPhase::Terminating | ProxyCallPhase::Ended
            ) {
                inner.phase = ProxyCallPhase::Terminating;
            }
            inner.phase = ProxyCallPhase::Ended;
            true
        });
    }

    pub fn set_current_target(&self, target: Option<String>) {
        let target_clone = target.clone();
        let changed = self.update(|inner| {
            if inner.current_target == target_clone {
                return false;
            }
            inner.current_target = target_clone.clone();
            true
        });
        if changed {
            self.emit_custom_event(ProxyCallEvent::TargetRinging {
                session_id: self.session_id(),
                target,
            });
        }
    }

    pub fn set_queue_name(&self, queue: Option<String>) {
        self.update(|inner| {
            if inner.queue_name == queue {
                return false;
            }
            inner.queue_name = queue;
            true
        });
    }

    fn update<F>(&self, mutate: F) -> bool
    where
        F: FnOnce(&mut CallSessionSnapshot) -> bool,
    {
        let mut inner = self.inner.write().unwrap();
        let prev_phase = inner.phase;
        let prev_queue = inner.queue_name.clone();
        let prev_hangup = inner.hangup_reason.clone();
        let changed = mutate(&mut inner);
        if !changed {
            return false;
        }
        if let Some(registry) = self.registry.as_ref().and_then(|r| r.upgrade()) {
            registry.update(&inner.session_id, |entry| {
                entry.caller = inner.caller.clone();
                entry.callee = inner.callee.clone();
                entry.answered_at = inner.answer_time;
                entry.status = match inner.phase {
                    ProxyCallPhase::Bridged => ActiveProxyCallStatus::Talking,
                    _ => ActiveProxyCallStatus::Ringing,
                };
            });
        }

        let phase_event = if inner.phase != prev_phase {
            Some((inner.session_id.clone(), inner.phase))
        } else {
            None
        };

        let queue_event = if inner.queue_name != prev_queue {
            Some((
                inner.session_id.clone(),
                prev_queue,
                inner.queue_name.clone(),
            ))
        } else {
            None
        };

        let hangup_event = if inner.hangup_reason != prev_hangup {
            Some((inner.session_id.clone(), inner.hangup_reason.clone()))
        } else {
            None
        };

        drop(inner);

        if let Some((session_id, phase)) = phase_event {
            self.emit_custom_event(ProxyCallEvent::PhaseChanged { session_id, phase });
        }

        if let Some((session_id, previous, current)) = queue_event {
            match (previous, current) {
                (Some(prev), Some(curr)) => {
                    self.emit_custom_event(ProxyCallEvent::QueueLeft {
                        session_id: session_id.clone(),
                        name: Some(prev),
                    });
                    self.emit_custom_event(ProxyCallEvent::QueueEntered {
                        session_id,
                        name: curr,
                    });
                }
                (Some(prev), None) => {
                    self.emit_custom_event(ProxyCallEvent::QueueLeft {
                        session_id,
                        name: Some(prev),
                    });
                }
                (None, Some(curr)) => {
                    self.emit_custom_event(ProxyCallEvent::QueueEntered {
                        session_id,
                        name: curr,
                    });
                }
                (None, None) => {}
            }
        }

        if let Some((session_id, reason)) = hangup_event {
            self.emit_custom_event(ProxyCallEvent::Hangup { session_id, reason });
        }

        true
    }

    fn event_sender(&self) -> Option<ProxyCallEventSender> {
        self.events.read().ok().and_then(|opt| opt.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProxyCallEvent {
    PhaseChanged {
        session_id: String,
        phase: ProxyCallPhase,
    },
    TargetRinging {
        session_id: String,
        target: Option<String>,
    },
    TargetAnswered {
        session_id: String,
        callee: Option<String>,
    },
    TargetFailed {
        session_id: String,
        target: Option<String>,
        code: Option<u16>,
        reason: Option<String>,
    },
    QueueEntered {
        session_id: String,
        name: String,
    },
    QueueLeft {
        session_id: String,
        name: Option<String>,
    },
    Hangup {
        session_id: String,
        reason: Option<CallRecordHangupReason>,
    },
}

pub type SessionActionSender = mpsc::UnboundedSender<SessionAction>;
pub type SessionActionReceiver = mpsc::UnboundedReceiver<SessionAction>;
pub type ProxyCallEventSender = mpsc::UnboundedSender<ProxyCallEvent>;

#[derive(Clone)]
pub struct CallSessionHandle {
    session_id: String,
    shared: CallSessionShared,
    cmd_tx: SessionActionSender,
}

impl CallSessionHandle {
    pub fn with_shared(shared: CallSessionShared) -> (Self, SessionActionReceiver) {
        let session_id = shared.session_id();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        shared.set_event_sender(event_tx);
        let handle = Self {
            session_id,
            shared,
            cmd_tx,
        };
        (handle, cmd_rx)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn snapshot(&self) -> CallSessionSnapshot {
        self.shared.snapshot()
    }

    pub fn send_command(&self, action: SessionAction) -> Result<()> {
        self.cmd_tx.send(action).map_err(Into::into)
    }

    pub fn set_queue_name(&self, queue: Option<String>) {
        self.shared.set_queue_name(queue)
    }

    pub fn queue_name(&self) -> Option<String> {
        self.shared.queue_name()
    }

    /// Send a [`ControllerEvent`] directly to the running `CallApp` event loop.
    ///
    /// Returns `true` if the event was delivered (i.e. an app is currently running
    /// on this call and the channel is open).
    pub fn send_app_event(&self, event: crate::call::app::ControllerEvent) -> bool {
        self.shared.send_app_event(event)
    }

    pub fn cancel_dtmf_listener(&self) {
        self.shared.cancel_dtmf_listener();
    }

    pub fn shared_exported_leg_media(&self) -> SharedExportedLegMediaRef {
        self.shared.shared_exported_leg_media()
    }

    pub fn publish_exported_leg_media(
        &self,
        peer: Arc<dyn MediaPeer>,
        offer_sdp: Option<String>,
        answer_sdp: Option<String>,
        negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
        ssrc: Option<u32>,
        is_bridged: bool,
    ) {
        self.shared.publish_exported_leg_media(
            peer,
            offer_sdp,
            answer_sdp,
            negotiated_audio,
            ssrc,
            is_bridged,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;

    fn make_handle(session_id: &str) -> (CallSessionHandle, SessionActionReceiver) {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            DialDirection::Inbound,
            Some("caller".to_string()),
            Some("callee".to_string()),
            None,
        );
        CallSessionHandle::with_shared(shared)
    }

    // ── ProxyCallPhase transition validation ────────────────────────────────

    #[test]
    fn test_phase_valid_transitions() {
        use super::ProxyCallPhase::*;
        // Normal call flow
        assert!(Initializing.can_transition_to(Ringing));
        assert!(Ringing.can_transition_to(EarlyMedia));
        assert!(EarlyMedia.can_transition_to(Bridged));
        assert!(Bridged.can_transition_to(Terminating));
        assert!(Terminating.can_transition_to(Ended));
    }

    #[test]
    fn test_phase_invalid_transitions() {
        use super::ProxyCallPhase::*;
        assert!(!Ended.can_transition_to(Initializing));
        assert!(!Ended.can_transition_to(Bridged));
        assert!(!Bridged.can_transition_to(Ringing));
        assert!(!Terminating.can_transition_to(Bridged));
    }

    #[test]
    fn test_phase_is_terminal() {
        use super::ProxyCallPhase::*;
        assert!(Ended.is_terminal());
        assert!(!Bridged.is_terminal());
        assert!(!Terminating.is_terminal());
    }

    #[test]
    fn test_transition_to_terminating() {
        let shared = CallSessionShared::new(
            "term-test".to_string(),
            DialDirection::Inbound,
            Some("caller".to_string()),
            Some("callee".to_string()),
            None,
        );
        // Initially Initializing → can transition to Terminating
        assert!(shared.transition_to_terminating());
        // Already Terminating → returns false
        assert!(!shared.transition_to_terminating());
    }

    // ── SessionAction serialization / equality ─────────────────────────────

    #[test]
    fn test_session_action_bridge_to_serde() {
        let action = SessionAction::BridgeTo {
            target_session_id: "session-b".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let decoded: SessionAction = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, SessionAction::BridgeTo { target_session_id } if target_session_id == "session-b")
        );
    }

    #[test]
    fn test_session_action_unbridge_serde() {
        let action = SessionAction::Unbridge;
        let json = serde_json::to_string(&action).unwrap();
        let decoded: SessionAction = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, SessionAction::Unbridge);
    }

    #[test]
    fn test_session_action_stop_playback_serde() {
        let action = SessionAction::StopPlayback;
        let json = serde_json::to_string(&action).unwrap();
        let decoded: SessionAction = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, SessionAction::StopPlayback);
    }

    // ── send_command / receive round-trips ─────────────────────────────────

    #[test]
    fn test_send_bridge_to_via_handle() {
        let (handle, mut rx) = make_handle("s-bridge");
        handle
            .send_command(SessionAction::BridgeTo {
                target_session_id: "s-other".to_string(),
            })
            .expect("send should succeed");

        let received = rx.try_recv().expect("should have one message");
        assert!(
            matches!(received, SessionAction::BridgeTo { target_session_id } if target_session_id == "s-other")
        );
    }

    #[test]
    fn test_send_unbridge_via_handle() {
        let (handle, mut rx) = make_handle("s-unbridge");
        handle
            .send_command(SessionAction::Unbridge)
            .expect("send should succeed");

        let received = rx.try_recv().expect("should have one message");
        assert_eq!(received, SessionAction::Unbridge);
    }

    #[test]
    fn test_send_stop_playback_via_handle() {
        let (handle, mut rx) = make_handle("s-stop");
        handle
            .send_command(SessionAction::StopPlayback)
            .expect("send should succeed");

        let received = rx.try_recv().expect("should have one message");
        assert_eq!(received, SessionAction::StopPlayback);
    }

    // ── ordering: multiple commands are queued in order ────────────────────

    #[test]
    fn test_command_queue_ordering() {
        let (handle, mut rx) = make_handle("s-order");

        handle.send_command(SessionAction::StopPlayback).unwrap();
        handle.send_command(SessionAction::Unbridge).unwrap();
        handle
            .send_command(SessionAction::BridgeTo {
                target_session_id: "target".to_string(),
            })
            .unwrap();

        assert_eq!(rx.try_recv().unwrap(), SessionAction::StopPlayback);
        assert_eq!(rx.try_recv().unwrap(), SessionAction::Unbridge);
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionAction::BridgeTo { .. }
        ));
        assert!(rx.try_recv().is_err(), "queue should be empty");
    }

    // ── send fails after receiver is dropped ──────────────────────────────

    #[test]
    fn test_send_fails_after_receiver_dropped() {
        let (handle, rx) = make_handle("s-closed");
        drop(rx); // close the receiving end

        let result = handle.send_command(SessionAction::StopPlayback);
        assert!(
            result.is_err(),
            "send_command should fail when receiver is dropped"
        );
    }
}
