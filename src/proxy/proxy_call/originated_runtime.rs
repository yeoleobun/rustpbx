use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::call_leg::{CallLeg, CallLegDirection};
use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use crate::proxy::proxy_call::media_peer::{MediaPeer, VoiceEnginePeer};
use crate::proxy::proxy_call::reporter::CallReporter;
use crate::proxy::proxy_call::session::{
    CallSession, OriginatedDialParams, OriginatedSessionEvent,
};
use crate::proxy::proxy_call::state::{
    CallContext, CallSessionHandle, CallSessionShared, MidDialogLeg,
};
use crate::proxy::server::SipServerRef;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use rsip::StatusCode;
use rsip::Uri;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::dialog::invitation::InviteOption;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::session::SessionActionInbox;

pub(crate) struct OriginatedRuntime;

impl OriginatedRuntime {
    /// Run the originated dial flow: create target leg, send INVITE, handle
    /// dialog state transitions until answered or failed.
    pub async fn run_dial(
        session: &mut CallSession,
        action_inbox: &mut SessionActionInbox,
    ) -> Result<()> {
        let params = session
            .originated_dial_params
            .take()
            .ok_or_else(|| anyhow!("originated_dial_params missing"))?;

        session.shared.transition_to_dialing();

        // Create the target leg now that we're about to dial
        let offer_sdp = params
            .invite_option
            .offer
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string());
        let target_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", session.context.session_id))
            .with_cancel_token(session.cancel_token.child_token());
        let target_peer: Arc<dyn MediaPeer> =
            Arc::new(VoiceEnginePeer::new(Arc::new(target_media_builder.build())));
        session.target_leg = Some(CallLeg::new(
            session.context.session_id.clone(),
            crate::proxy::proxy_call::call_leg::LegRole::Callee,
            CallLegDirection::Outbound,
            target_peer,
            offer_sdp,
        ));
        // Set dialog event channel from stored tx
        if let Some(tx) = session.target_dialog_event_tx.take() {
            session.target_leg_mut().sip.dialog_event_tx = Some(tx);
        }

        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let dialog_layer = session.dialog_layer.clone();
        let invite_future = dialog_layer.do_invite(params.invite_option, state_tx);
        tokio::pin!(invite_future);

        let mut timeout = tokio::time::sleep(Duration::from_secs(params.timeout_secs)).boxed();
        let mut invite_completed = false;
        let mut client_dialog: Option<rsipstack::dialog::client_dialog::ClientInviteDialog> = None;

        use futures::FutureExt;

        loop {
            // Process any pending session actions
            while let Ok(action) = action_inbox.try_recv() {
                session.apply_session_action(action, None).await?;
            }

            tokio::select! {
                // Timeout before answer
                _ = &mut timeout, if !invite_completed => {
                    session.set_error(StatusCode::RequestTimeout, Some("No answer".to_string()), None);
                    session.hangup_reason = Some(CallRecordHangupReason::NoAnswer);
                    Self::emit_event(&params.event_tx, OriginatedSessionEvent::Failed {
                        reason: "no_answer".to_string(),
                        sip_status: Some(408),
                    });
                    session.cancel_token.cancel();
                    return Err(anyhow!("Originate timeout"));
                }

                // Dialog state transitions from the callee
                state = state_rx.recv() => {
                    match state {
                        Some(DialogState::Calling(_)) => {
                            session.shared.transition_to_ringing(false);
                            session.ring_time = Some(Instant::now());
                            Self::emit_event(&params.event_tx, OriginatedSessionEvent::Ringing);
                        }
                        Some(DialogState::Early(_, _)) => {
                            session.shared.transition_to_ringing(true);
                            if session.ring_time.is_none() {
                                session.ring_time = Some(Instant::now());
                            }
                            Self::emit_event(&params.event_tx, OriginatedSessionEvent::EarlyMedia);
                        }
                        Some(DialogState::Confirmed(dialog_id, _)) => {
                            debug!(session_id = %session.context.session_id, %dialog_id, "Originated callee dialog confirmed");
                        }
                        Some(DialogState::Terminated(_, _)) => {
                            info!(session_id = %session.context.session_id, "Originated callee dialog terminated");
                            if session.answer_time.is_some() {
                                session.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                                Self::emit_event(&params.event_tx, OriginatedSessionEvent::Hangup {
                                    reason: "terminated".to_string(),
                                });
                            } else {
                                session.hangup_reason = Some(CallRecordHangupReason::Failed);
                                Self::emit_event(&params.event_tx, OriginatedSessionEvent::Failed {
                                    reason: "terminated".to_string(),
                                    sip_status: None,
                                });
                            }
                            session.cancel_token.cancel();
                            return Ok(());
                        }
                        Some(DialogState::Updated(dialog_id, request, tx_handle)) => {
                            debug!(session_id = %session.context.session_id, %dialog_id, "Originated call received mid-dialog update");
                            let is_reinvite = *request.method() == rsip::Method::Invite;
                            let is_update = *request.method() == rsip::Method::Update;
                            if is_reinvite || is_update {
                                let has_sdp = !request.body().is_empty();
                                let sdp = has_sdp.then(|| String::from_utf8_lossy(request.body()).to_string());
                                let method = request.method().clone();
                                session.shared.store_mid_dialog_reply(
                                    MidDialogLeg::Callee,
                                    &dialog_id.to_string(),
                                    tx_handle,
                                );
                                session.handle_reinvite(
                                    MidDialogLeg::Callee,
                                    method,
                                    dialog_id.to_string(),
                                    sdp,
                                ).await;
                            }
                        }
                        Some(_) => {
                            // Other dialog states (Trying, WaitAck, etc.) — ignore
                        }
                        None => {
                            debug!(session_id = %session.context.session_id, "Originated callee state channel closed");
                            break;
                        }
                    }
                }

                // INVITE future completion (initial response)
                result = &mut invite_future, if !invite_completed => {
                    invite_completed = true;
                    match result {
                        Ok((dialog, Some(resp))) if resp.status_code.kind() == rsip::StatusCodeKind::Successful => {
                            info!(session_id = %session.context.session_id, dialog_id = %dialog.id(), "Originated call answered (200 OK)");
                            session.answer_time = Some(Instant::now());
                            session.connected_callee = Some(session.context.original_callee.clone());

                            // Register callee dialog
                            let dialog_id = dialog.id();
                            {
                                let mut active = session.target_leg_mut().sip.active_dialog_ids.lock().unwrap();
                                active.insert(dialog_id.clone());
                            }
                            session.target_leg_mut().sip.connected_dialog_id = Some(dialog_id.clone());
                            if let Some(ref handle) = session.handle {
                                session.shared.register_dialog(dialog_id.to_string(), handle.clone());
                            }

                            // Store answer SDP from the remote on both legs
                            if !resp.body().is_empty() {
                                let answer_sdp = String::from_utf8_lossy(resp.body()).to_string();
                                session.target_leg_mut().media.answer_sdp = Some(answer_sdp.clone());
                                session.exported_leg.media.answer_sdp = Some(answer_sdp.clone());
                                session.exported_leg.media.select_or_store_negotiated_audio(
                                    MediaEndpoint::select_best_audio_from_sdp(
                                        &answer_sdp,
                                        &session.context.dialplan.allow_codecs,
                                    ),
                                );
                            }

                            // Publish caller media (stable identity) to shared state
                            session.publish_exported_leg_media();

                            session.shared.transition_to_answered();
                            Self::emit_event(&params.event_tx, OriginatedSessionEvent::Answered);
                            client_dialog = Some(dialog);
                        }
                        Ok((_dialog, resp_opt)) => {
                            let sip_status = resp_opt.as_ref().map(|r| r.status_code.code());
                            let is_busy = sip_status == Some(486) || sip_status == Some(600);
                            session.hangup_reason = Some(CallRecordHangupReason::Failed);

                            if is_busy {
                                Self::emit_event(&params.event_tx, OriginatedSessionEvent::Busy);
                            } else {
                                Self::emit_event(&params.event_tx, OriginatedSessionEvent::Failed {
                                    reason: "originate_failed".to_string(),
                                    sip_status,
                                });
                            }

                            let code = sip_status
                                .and_then(|c| StatusCode::try_from(c).ok())
                                .unwrap_or(StatusCode::ServerInternalError);
                            session.set_error(code, Some("Originate failed".to_string()), None);
                            session.cancel_token.cancel();
                            return Err(anyhow!("Originate failed"));
                        }
                        Err(error) => {
                            session.hangup_reason = Some(CallRecordHangupReason::Failed);
                            Self::emit_event(&params.event_tx, OriginatedSessionEvent::Failed {
                                reason: error.to_string(),
                                sip_status: None,
                            });
                            session.set_error(StatusCode::ServerInternalError, Some(error.to_string()), None);
                            session.cancel_token.cancel();
                            return Err(error.into());
                        }
                    }
                }

                // Cancellation
                _ = session.cancel_token.cancelled() => {
                    debug!(session_id = %session.context.session_id, "Originated call cancelled");
                    if let Some(ref dialog) = client_dialog {
                        let _ = dialog.hangup().await;
                    }
                    if session.hangup_reason.is_none() {
                        session.hangup_reason = Some(CallRecordHangupReason::Canceled);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Hold the callee side for originated/app-driven sessions.
    /// Sends a re-INVITE with `sendonly` to the remote party and optionally
    /// plays hold music on the caller peer.
    pub async fn hold_callee(session: &mut CallSession, music_source: Option<&str>) -> Result<()> {
        use rsipstack::dialog::dialog::Dialog;

        let callee_dialog_id = session
            .target_leg()
            .sip
            .connected_dialog_id
            .as_ref()
            .ok_or_else(|| anyhow!("no connected callee dialog for hold"))?
            .clone();

        let dialog = session
            .dialog_layer
            .get_dialog(&callee_dialog_id)
            .ok_or_else(|| anyhow!("callee dialog not found for hold"))?;

        let Dialog::ClientInvite(client_dialog) = dialog else {
            return Err(anyhow!("callee dialog is not a client invite dialog"));
        };

        // Build a sendonly SDP offer from the caller peer's track
        let offer_sdp = Self::build_local_offer_with_direction(session, "sendonly").await?;
        session.exported_leg.media.offer_sdp = Some(offer_sdp.clone());

        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        let response = client_dialog
            .reinvite(Some(headers), Some(offer_sdp.into_bytes()))
            .await
            .map_err(|e| anyhow!("hold re-INVITE failed: {}", e))?;

        if let Some(resp) = response {
            if !resp.body().is_empty() {
                let answer_sdp = String::from_utf8_lossy(resp.body()).to_string();
                session.target_leg_mut().media.answer_sdp = Some(answer_sdp.clone());
                session.exported_leg.media.answer_sdp = Some(answer_sdp.clone());
                session.exported_leg.media.select_or_store_negotiated_audio(
                    MediaEndpoint::select_best_audio_from_sdp(
                        &answer_sdp,
                        &session.context.dialplan.allow_codecs,
                    ),
                );
            }
        }

        // Re-publish caller media state after hold (stable identity)
        session.publish_exported_leg_media();

        // Suppress media forwarding on the caller peer
        let tracks = session.exported_leg.media.peer.get_tracks().await;
        if let Some(track) = tracks.first() {
            let guard = track.lock().await;
            session
                .exported_leg
                .media
                .peer
                .suppress_forwarding(guard.id())
                .await;
        }

        // Play hold music if provided
        if let Some(audio_file) = music_source.filter(|s| !s.is_empty()) {
            session
                .exported_leg
                .play_audio(
                    &session.context.session_id,
                    audio_file,
                    true,
                    "hold_music",
                    true,
                    session.cancel_token.clone(),
                    session.app_event_tx.clone(),
                )
                .await?;
        }

        Ok(())
    }

    /// Unhold the callee side for originated/app-driven sessions.
    /// Sends a re-INVITE with `sendrecv` to the remote party.
    pub async fn unhold_callee(session: &mut CallSession) -> Result<()> {
        use rsipstack::dialog::dialog::Dialog;

        let callee_dialog_id = session
            .target_leg()
            .sip
            .connected_dialog_id
            .as_ref()
            .ok_or_else(|| anyhow!("no connected callee dialog for unhold"))?
            .clone();

        let dialog = session
            .dialog_layer
            .get_dialog(&callee_dialog_id)
            .ok_or_else(|| anyhow!("callee dialog not found for unhold"))?;

        let Dialog::ClientInvite(client_dialog) = dialog else {
            return Err(anyhow!("callee dialog is not a client invite dialog"));
        };

        let offer_sdp = Self::build_local_offer_with_direction(session, "sendrecv").await?;
        session.exported_leg.media.offer_sdp = Some(offer_sdp.clone());

        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        let response = client_dialog
            .reinvite(Some(headers), Some(offer_sdp.into_bytes()))
            .await
            .map_err(|e| anyhow!("unhold re-INVITE failed: {}", e))?;

        if let Some(resp) = response {
            if !resp.body().is_empty() {
                let answer_sdp = String::from_utf8_lossy(resp.body()).to_string();
                session.target_leg_mut().media.answer_sdp = Some(answer_sdp.clone());
                session.exported_leg.media.answer_sdp = Some(answer_sdp.clone());
                session.exported_leg.media.select_or_store_negotiated_audio(
                    MediaEndpoint::select_best_audio_from_sdp(
                        &answer_sdp,
                        &session.context.dialplan.allow_codecs,
                    ),
                );
            }
        }

        // Re-publish caller media state after unhold (stable identity)
        session.publish_exported_leg_media();

        // Remove hold music and resume forwarding
        session
            .exported_leg
            .media
            .peer
            .remove_track("hold_music", true)
            .await;
        let tracks = session.exported_leg.media.peer.get_tracks().await;
        if let Some(track) = tracks.first() {
            let guard = track.lock().await;
            session
                .exported_leg
                .media
                .peer
                .resume_forwarding(guard.id())
                .await;
        }

        Ok(())
    }

    /// Transfer via SIP REFER for originated/app-driven sessions.
    pub async fn refer_callee(session: &mut CallSession, target: &str) -> Result<()> {
        use rsipstack::dialog::dialog::Dialog;

        let target_uri = Uri::try_from(target)
            .map_err(|e| anyhow!("invalid transfer target '{}': {}", target, e))?;

        let callee_dialog_id = session
            .target_leg()
            .sip
            .connected_dialog_id
            .as_ref()
            .ok_or_else(|| anyhow!("no connected callee dialog for transfer"))?
            .clone();

        let dialog = session
            .dialog_layer
            .get_dialog(&callee_dialog_id)
            .ok_or_else(|| anyhow!("callee dialog not found for transfer"))?;

        let Dialog::ClientInvite(client_dialog) = dialog else {
            return Err(anyhow!("callee dialog is not a client invite dialog"));
        };

        let result = client_dialog
            .refer(target_uri, None, None)
            .await
            .map_err(|e| anyhow!("REFER failed: {}", e))?;

        match result {
            Some(resp) if resp.status_code.kind() == rsip::StatusCodeKind::Successful => {
                info!(
                    session_id = %session.context.session_id,
                    target = %target,
                    "Originated call transferred successfully"
                );
                Ok(())
            }
            Some(resp) => Err(anyhow!("REFER rejected with status {}", resp.status_code)),
            None => Err(anyhow!("REFER returned no response")),
        }
    }

    /// Build a local SDP offer from the caller peer's first track with
    /// the specified media direction.
    async fn build_local_offer_with_direction(
        session: &CallSession,
        direction: &str,
    ) -> Result<String> {
        let tracks = session.exported_leg.media.peer.get_tracks().await;
        let track = tracks
            .first()
            .ok_or_else(|| anyhow!("no track on caller peer for SDP offer"))?;
        let guard = track.lock().await;
        let offer = guard
            .local_description()
            .await
            .map_err(|e| anyhow!("failed to get local description: {}", e))?;

        let dir = match direction {
            "sendonly" => rustrtc::Direction::SendOnly,
            "recvonly" => rustrtc::Direction::RecvOnly,
            "inactive" => rustrtc::Direction::Inactive,
            _ => rustrtc::Direction::SendRecv,
        };

        let mut desc = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer)
            .map_err(|e| anyhow!("failed to parse local SDP: {:?}", e))?;

        for section in &mut desc.media_sections {
            if section.kind == rustrtc::MediaKind::Audio {
                section.direction = dir;
            }
        }

        Ok(desc.to_sdp_string())
    }

    /// Helper to emit an originated session event if a sender is available.
    fn emit_event(
        tx: &Option<mpsc::UnboundedSender<OriginatedSessionEvent>>,
        event: OriginatedSessionEvent,
    ) {
        if let Some(tx) = tx {
            let _ = tx.send(event);
        }
    }

    // ── Bootstrap ────────────────────────────────────────────────────

    /// Bootstrap an RWI-originated session: no inbound server dialog,
    /// single outbound target dialed directly.
    ///
    /// Returns the session handle and shared state so the RWI layer can
    /// send commands and observe state.
    pub async fn serve(
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
        use crate::call::{DialDirection, DialStrategy, Dialplan, DialplanFlow, MediaConfig};
        use crate::proxy::proxy_call::state::SessionKind;

        // Build a synthetic request for reporting — minimal INVITE with caller/callee URIs
        let caller_uri_str = caller_display
            .clone()
            .unwrap_or_else(|| "sip:rwi@local".to_string());
        let callee_uri_str = callee_display
            .clone()
            .unwrap_or_else(|| invite_option.callee.to_string());
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
}
