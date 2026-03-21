use crate::call::RingbackMode;
use crate::proxy::proxy_call::caller_negotiation;
use crate::proxy::proxy_call::session::CallSession;
use crate::proxy::proxy_call::sip_leg::TRICKLE_ICE_TAG;
use crate::proxy::proxy_call::state::ProxyCallEvent;
use anyhow::{Result, anyhow};
use rsip::StatusCode;
use rsipstack::dialog::DialogId;
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

pub(crate) struct AnswerRuntime;

impl AnswerRuntime {
    fn trickle_ice_enabled(session: &CallSession) -> bool {
        session.caller_leg.sip.supports_trickle_ice
            && session
                .caller_leg
                .media.offer_sdp
                .as_deref()
                .map(CallSession::is_webrtc_sdp)
                .unwrap_or(false)
    }

    async fn spawn_inbound_trickle_ice_sender(session: &CallSession) {
        let server_dialog = session.server_dialog.clone();
        let cancel_token = session.cancel_token.child_token();
        let session_id = session.context.session_id.clone();
        let Some((_pc, mut candidate_rx, mut gathering_rx)) =
            session.caller_leg.media.trickle_ice_context("caller-track").await
        else {
            return;
        };
        crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    candidate = candidate_rx.recv() => match candidate {
                        Ok(candidate) => {
                            let body = format!("a=mid:0\r\na=candidate:{}\r\n", candidate.to_sdp());
                            let headers = vec![
                                rsip::Header::ContentType("application/trickle-ice-sdpfrag".into()),
                            ];
                            if let Err(err) = server_dialog.info(Some(headers), Some(body.into_bytes())).await {
                                warn!(session_id = %session_id, error = %err, "Failed to send local trickle ICE candidate");
                            }
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                    changed = gathering_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        if *gathering_rx.borrow() == rustrtc::IceGatheringState::Complete {
                            let headers = vec![
                                rsip::Header::ContentType("application/trickle-ice-sdpfrag".into()),
                            ];
                            let body = "a=mid:0\r\na=end-of-candidates\r\n".as_bytes().to_vec();
                            if let Err(err) = server_dialog.info(Some(headers), Some(body)).await {
                                warn!(session_id = %session_id, error = %err, "Failed to send end-of-candidates");
                            }
                            break;
                        }
                    }
                }
            }
        });
    }

    pub async fn start_ringing(session: &mut CallSession, answer: String) {
        Self::start_ringing_internal(session, answer, None).await;
    }

    async fn start_ringing_internal(
        session: &mut CallSession,
        mut answer: String,
        dialog_id: Option<DialogId>,
    ) {
        let call_answered = session.answer_time.is_some();
        if call_answered {
            return;
        }

        if session.ring_time.is_some() && answer.is_empty() && dialog_id.is_none() {
            debug!("Ringing already sent, skipping duplicate 180");
            return;
        }

        if session.caller_leg.media.early_media_sent && dialog_id.is_none() {
            debug!("Early media already sent, skipping ringing");
            return;
        }

        session.shared.transition_to_ringing(!answer.is_empty());

        if session.ring_time.is_none() {
            session.ring_time = Some(Instant::now());
        }

        let has_early_media = !answer.is_empty();
        let ringback_mode = session.context.dialplan.ringback.mode;

        let should_play_local = match ringback_mode {
            RingbackMode::Local => true,
            RingbackMode::Passthrough => false,
            RingbackMode::Auto => !has_early_media,
            RingbackMode::None => false,
        };

        let should_passthrough = match ringback_mode {
            RingbackMode::Local => false,
            RingbackMode::Passthrough => has_early_media,
            RingbackMode::Auto => has_early_media,
            RingbackMode::None => false,
        };

        if has_early_media {
            session.caller_leg.media.early_media_sent = true;

            if session.use_media_proxy {
                if should_passthrough {
                    info!(
                        session_id = %session.context.session_id,
                        mode = ?ringback_mode,
                        "Forwarding callee early media to caller (passthrough mode)"
                    );
                    let _ = session.setup_callee_track(&answer, dialog_id.as_ref()).await;
                }

                let caller_codec_info = caller_negotiation::build_final_caller_codec_info(
                    session.caller_leg.media.offer_sdp.as_deref(),
                    &answer,
                    &session.context.dialplan.allow_codecs,
                    session.use_media_proxy,
                );
                match caller_codec_info {
                    Ok(codec_info) => match session.build_caller_answer(codec_info).await {
                        Ok(answer_for_caller) => {
                            let callee_early_sdp = answer.clone();
                            session.set_answer(answer_for_caller.clone());
                            answer = answer_for_caller;

                            if session.bridge_runtime.media_bridge.is_none() {
                                info!(
                                    session_id = %session.context.session_id,
                                    "Creating media bridge during early media (183)"
                                );
                                session
                                    .ensure_media_bridge_from_sdp(
                                        &callee_early_sdp,
                                        true,
                                        "early-media",
                                    )
                                    .await;
                                if let Err(e) = session.bridge_runtime.start_bridge().await {
                                    warn!(
                                        session_id = %session.context.session_id,
                                        "Failed to start media bridge during early media: {}", e
                                    );
                                }
                            }

                            if !call_answered && should_play_local {
                                if let Some(ref file_name) = session.context.dialplan.ringback.audio_file {
                                    info!(
                                        session_id = %session.context.session_id,
                                        mode = ?ringback_mode,
                                        audio_file = %file_name,
                                        "Playing local ringback (local/auto mode with early media)"
                                    );
                                    let loop_playback =
                                        session.context.dialplan.ringback.loop_playback;
                                    session
                                        .caller_leg
                                        .media
                                        .create_file_track(
                                            CallSession::RINGBACK_TRACK_ID,
                                            file_name,
                                            loop_playback,
                                        )
                                        .await;
                                    let track_id = if let Some(ref id) = dialog_id {
                                        format!("callee-track-{}", id)
                                    } else {
                                        CallSession::CALLEE_TRACK_ID.to_string()
                                    };
                                    session.caller_leg.media.peer.suppress_forwarding(&track_id).await;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to negotiate final codec: {}", e);
                            return;
                        }
                    },
                    Err(e) => {
                        warn!("Failed to negotiate final codec: {}", e);
                        return;
                    }
                }
            }
        } else if should_play_local {
            if let Some(ref file_name) = session.context.dialplan.ringback.audio_file {
                info!(
                    session_id = %session.context.session_id,
                    mode = ?ringback_mode,
                    audio_file = %file_name,
                    "Playing local ringback (no callee early media)"
                );

                if session.use_media_proxy {
                    let loop_playback = session.context.dialplan.ringback.loop_playback;
                    session
                        .caller_leg
                        .media
                        .create_file_track(
                            CallSession::RINGBACK_TRACK_ID,
                            file_name,
                            loop_playback,
                        )
                        .await;
                }
            }
        }

        if call_answered {
            return;
        }

        let status_code = if has_early_media {
            StatusCode::SessionProgress
        } else {
            StatusCode::Ringing
        };

        let (headers, body) = if has_early_media && should_passthrough {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            (Some(headers), Some(answer.into_bytes()))
        } else {
            (None, None)
        };

        if let Err(e) = session.server_dialog.ringing(headers, body) {
            warn!("Failed to send {} response: {}", status_code, e);
            return;
        }
    }

    pub async fn accept_call(
        session: &mut CallSession,
        callee: Option<String>,
        callee_answer: Option<String>,
        dialog_id: Option<String>,
    ) -> Result<()> {
        let first_answer = session.answer_time.is_none();

        if let Some(callee_addr) = callee {
            let resolved_callee = session.routed_callee.clone().unwrap_or(callee_addr);
            session.connected_callee = Some(resolved_callee);
        }
        if let Some(ref id_str) = dialog_id {
            let matched_dialog_id = session
                .callee_leg
                .sip
                .recorded_dialogs()
                .into_iter()
                .find(|id| id.to_string() == *id_str);
            if let Some(id) = matched_dialog_id {
                session.callee_leg.sip.set_connected_dialog(id);
            }
        }
        if first_answer {
            session.answer_time = Some(Instant::now());
        }
        info!(
            server_dialog_id = %session.server_dialog.id(),
            use_media_proxy = session.use_media_proxy,
            has_answer = session.caller_leg.media.answer_sdp.is_some(),
            dialog_id = ?dialog_id,
            "Call answered"
        );

        if first_answer && !session.caller_leg.media.early_media_sent {
            if let Some(ref callee_sdp) = callee_answer {
                if let Some(codec_info) = caller_negotiation::build_optimized_caller_codec_info(
                    session.caller_leg.media.offer_sdp.as_deref(),
                    callee_sdp,
                    &session.context.dialplan.allow_codecs,
                    session.use_media_proxy,
                ) {
                    let build_result = if Self::trickle_ice_enabled(session) {
                        session.build_caller_answer_trickle(codec_info).await
                    } else {
                        session.build_caller_answer(codec_info).await
                    };
                    match build_result {
                        Ok(optimized_answer) => session.set_answer(optimized_answer),
                        Err(e) => warn!(
                            session_id = %session.context.session_id,
                            error = %e,
                            "Failed to build optimized caller answer"
                        ),
                    }
                }
            }
        }

        if session.caller_leg.media.answer_sdp.is_none() {
            let answer_for_caller = if Self::trickle_ice_enabled(session) {
                session
                    .build_caller_answer_trickle(
                        caller_negotiation::build_passthrough_caller_answer_codec_info(
                            session.caller_leg.media.offer_sdp.as_deref(),
                        ),
                    )
                    .await?
            } else {
                session
                    .build_caller_answer(
                        caller_negotiation::build_passthrough_caller_answer_codec_info(
                            session.caller_leg.media.offer_sdp.as_deref(),
                        ),
                    )
                    .await?
            };
            session.set_answer(answer_for_caller);
        }

        if first_answer {
            session.freeze_answered_caller_audio();
        }

        if session.use_media_proxy {
            let track_id = CallSession::CALLEE_TRACK_ID.to_string();
            session
                .caller_leg
                .media
                .remove_ringback_track(CallSession::RINGBACK_TRACK_ID)
                .await;
            if let Some(answer) = callee_answer.as_ref() {
                session.setup_callee_track(answer, None).await?;

                if session.bridge_runtime.media_bridge.is_none() {
                    session.caller_leg.media.cancel_dtmf_listener();
                    session.shared.set_dtmf_listener_cancel(None);
                    session
                        .ensure_media_bridge_from_sdp(answer, false, "final-answer")
                        .await;
                }
            }

            if let Err(e) = session.bridge_runtime.start_bridge().await {
                warn!(session_id = %session.context.session_id, "Failed to start media bridge: {}", e);
            }
            let _ = session.bridge_runtime.resume_forwarding(&track_id).await;
        } else {
            if let Some(answer) = callee_answer {
                session.set_answer(answer);
            }
            session.bridge_runtime.stop_bridge();
            session.caller_leg.media.peer.stop();
            session.callee_leg.media.peer.stop();
        }

        let mut headers = if session.caller_leg.media.answer_sdp.is_some() {
            vec![rsip::Header::ContentType("application/sdp".into())]
        } else {
            vec![]
        };

        {
            let server_timer = session.caller_leg.sip.session_timer.lock().unwrap();
            if server_timer.active {
                headers.push(rsip::Header::Supported(
                    rsip::headers::Supported::from(crate::proxy::proxy_call::session_timer::TIMER_TAG).into(),
                ));
                headers.push(rsip::Header::Other(
                    crate::proxy::proxy_call::session_timer::HEADER_SESSION_EXPIRES.into(),
                    format!(
                        "{};refresher={}",
                        server_timer.session_interval.as_secs(),
                        server_timer.refresher
                    ),
                ));
            }
        }
        if session.caller_leg.sip.supports_trickle_ice {
            headers.push(rsip::Header::Supported(
                rsip::headers::Supported::from(TRICKLE_ICE_TAG).into(),
            ));
        }

        if let Err(e) = session.server_dialog.accept(
            Some(headers),
            session.caller_leg.media.answer_sdp.clone().map(|sdp| sdp.into_bytes()),
        ) {
            return Err(anyhow!("Failed to send 200 OK: {}", e));
        }
        if first_answer && Self::trickle_ice_enabled(session) {
            Self::spawn_inbound_trickle_ice_sender(session).await;
        }
        session.mark_active_call_answered();
        if first_answer {
            let callee = session
                .connected_callee
                .clone()
                .or_else(|| session.routed_callee.clone());
            session.shared.emit_custom_event(ProxyCallEvent::TargetAnswered {
                session_id: session.shared.session_id(),
                callee,
            });
        }
        session.publish_caller_media();
        Ok(())
    }
}
