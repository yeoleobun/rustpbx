use crate::call::app::ControllerEvent;
use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use crate::proxy::proxy_call::playback_runtime;
use crate::proxy::proxy_call::recording_runtime::{self, RecordingState};
use crate::proxy::proxy_call::session::NegotiationState;
use crate::proxy::proxy_call::sip_leg::SipLeg;
use anyhow::Result;
use rsip::StatusCode;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::server_dialog::ServerInviteDialog;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CallLegDirection {
    Inbound,
    Outbound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LegRole {
    Caller,
    Callee,
}

pub(crate) struct CallLeg {
    #[allow(dead_code)]
    pub id: String,
    pub role: LegRole,
    #[allow(dead_code)]
    pub direction: CallLegDirection,
    pub sip: SipLeg,
    pub media: MediaEndpoint,
    pub negotiation_state: NegotiationState,
}

impl CallLeg {
    pub fn new(
        id: String,
        role: LegRole,
        direction: CallLegDirection,
        peer: std::sync::Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer>,
        offer_sdp: Option<String>,
    ) -> Self {
        Self {
            id,
            role,
            direction,
            sip: SipLeg::new(direction),
            media: MediaEndpoint::new(peer, offer_sdp),
            negotiation_state: NegotiationState::Idle,
        }
    }

    /// Stop any active playback on this leg.
    pub async fn stop_playback(&self) {
        self.media.remove_track("prompt").await;
    }

    /// Remove hold music from this leg.
    pub async fn unhold(&self) {
        self.media.remove_track("hold_music").await;
    }

    /// Play an audio file on this leg.
    pub async fn play_audio(
        &self,
        session_id: &str,
        audio_file: &str,
        await_completion: bool,
        track_id: &str,
        loop_playback: bool,
        cancel_token: CancellationToken,
        app_event_tx: Option<mpsc::UnboundedSender<ControllerEvent>>,
    ) -> Result<()> {
        playback_runtime::play_audio_file(
            &self.media,
            session_id,
            audio_file,
            await_completion,
            track_id,
            loop_playback,
            cancel_token,
            app_event_tx,
        )
        .await
    }

    /// Start recording on this leg.
    pub async fn start_recording(
        &mut self,
        session_id: &str,
        recording_state: &mut RecordingState,
        path: &str,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> Result<()> {
        recording_runtime::start_recording(
            &mut self.media,
            session_id,
            recording_state,
            path,
            max_duration,
            beep,
        )
        .await
    }

    /// Stop recording on this leg.
    pub async fn stop_recording(
        &mut self,
        session_id: &str,
        recording_state: &mut RecordingState,
        app_event_tx: Option<mpsc::UnboundedSender<ControllerEvent>>,
    ) -> Result<()> {
        recording_runtime::stop_recording(
            &mut self.media,
            session_id,
            recording_state,
            app_event_tx,
        )
        .await
    }

    /// Hang up the inbound (server) dialog on this leg.
    pub async fn hangup_inbound(
        &self,
        server_dialog: &ServerInviteDialog,
        code: Option<StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        self.sip
            .hangup_inbound_dialog(server_dialog, code, reason)
            .await
    }

    /// Terminate all active client (outbound) dialogs on this leg.
    pub async fn terminate_client_dialogs(
        &self,
        session_id: &str,
        dialog_layer: &Arc<DialogLayer>,
    ) {
        let dialogs = self.sip.active_dialog_ids.lock().unwrap().clone();
        for dialog_id in dialogs {
            if let Some(dialog) = dialog_layer.get_dialog(&dialog_id) {
                debug!(%session_id, %dialog_id, role = ?self.role, "Terminating client dialog");
                dialog_layer.remove_dialog(&dialog_id);
                dialog.hangup().await.ok();
            }
        }
    }
}
