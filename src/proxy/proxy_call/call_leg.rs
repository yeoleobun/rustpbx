use crate::call::app::ControllerEvent;
use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use crate::proxy::proxy_call::playback_runtime;
use crate::proxy::proxy_call::recording_runtime::{self, RecordingState};
use crate::proxy::proxy_call::session::NegotiationState;
use crate::proxy::proxy_call::sip_leg::SipLeg;
use anyhow::Result;
use rsip::StatusCode;
use rsipstack::dialog::DialogId;
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

    // ── Playback / recording ──

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

    /// Set the server dialog on this leg (exported legs only).
    pub fn set_server_dialog(&mut self, server_dialog: ServerInviteDialog) {
        self.sip.server_dialog = Some(server_dialog);
    }

    /// Get a reference to the server dialog (exported legs only).
    pub fn server_dialog_ref(&self) -> Option<&ServerInviteDialog> {
        self.sip.server_dialog.as_ref()
    }

    /// Get the server dialog ID (exported legs only).
    pub fn server_dialog_id(&self) -> Option<DialogId> {
        self.sip.server_dialog.as_ref().map(|d| d.id())
    }

    /// Hang up the inbound (server) dialog on this leg.
    pub async fn hangup_inbound(
        &self,
        code: Option<StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        self.sip.hangup_inbound_dialog(code, reason).await
    }

    /// Send a provisional response (180/183) on the inbound server dialog.
    pub fn send_provisional(
        &self,
        headers: Option<Vec<rsip::Header>>,
        body: Option<Vec<u8>>,
    ) -> Result<()> {
        let server_dialog = self
            .sip
            .server_dialog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No server dialog on this leg"))?;
        server_dialog.ringing(headers, body)?;
        Ok(())
    }

    /// Send 200 OK (accept) on the inbound server dialog.
    pub fn accept_inbound(
        &self,
        headers: Option<Vec<rsip::Header>>,
        body: Option<Vec<u8>>,
    ) -> Result<()> {
        let server_dialog = self
            .sip
            .server_dialog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No server dialog on this leg"))?;
        server_dialog.accept(headers, body)?;
        Ok(())
    }

    /// Send BYE on the inbound server dialog.
    pub async fn bye_inbound(&self) -> Result<()> {
        let server_dialog = self
            .sip
            .server_dialog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No server dialog on this leg"))?;
        server_dialog.bye().await?;
        Ok(())
    }

    /// Reject the inbound server dialog with a status code.
    pub fn reject_inbound(&self, code: Option<StatusCode>, reason: Option<String>) -> Result<()> {
        let server_dialog = self
            .sip
            .server_dialog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No server dialog on this leg"))?;
        server_dialog.reject(code, reason)?;
        Ok(())
    }

    /// Check if the server dialog is terminated.
    pub fn is_server_dialog_terminated(&self) -> bool {
        self.sip
            .server_dialog
            .as_ref()
            .map(|d| d.state().is_terminated())
            .unwrap_or(true)
    }

    /// Check if the server dialog is confirmed.
    pub fn is_server_dialog_confirmed(&self) -> bool {
        self.sip
            .server_dialog
            .as_ref()
            .map(|d| d.state().is_confirmed())
            .unwrap_or(false)
    }

    /// Check if the server dialog is waiting for ACK.
    pub fn is_server_dialog_waiting_ack(&self) -> bool {
        self.sip
            .server_dialog
            .as_ref()
            .map(|d| d.state().waiting_ack())
            .unwrap_or(false)
    }

    /// Get the initial request from the server dialog.
    pub fn server_initial_request(&self) -> Option<rsip::Request> {
        self.sip.server_dialog.as_ref().map(|d| d.initial_request())
    }

    /// Clone the server dialog (for spawning tasks that need their own handle).
    pub fn clone_server_dialog(&self) -> Option<ServerInviteDialog> {
        self.sip.server_dialog.clone()
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
