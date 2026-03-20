use crate::proxy::proxy_call::session::{CallSession, PendingHangup};
use crate::proxy::proxy_call::session_timer::{
    HEADER_SESSION_EXPIRES, SessionRefresher, SessionTimerState, TIMER_TAG,
};
use crate::proxy::proxy_call::sip_leg::SipLeg;
use crate::proxy::proxy_call::state::{CallContext, CallSessionHandle, CallSessionShared, SessionAction};
use crate::callrecord::CallRecordHangupReason;
use anyhow::Result;
use rsipstack::dialog::{DialogId, dialog::DialogState, dialog_layer::DialogLayer, server_dialog::ServerInviteDialog};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub(crate) struct SessionLoopRuntime;

impl SessionLoopRuntime {
    fn is_trickle_ice_info(request: &rsip::Request) -> bool {
        request.headers.iter().any(|header| match header {
            rsip::Header::ContentType(value) => value
                .to_string()
                .eq_ignore_ascii_case("application/trickle-ice-sdpfrag"),
            rsip::Header::Other(name, value) if name.eq_ignore_ascii_case("Content-Type") => {
                value.eq_ignore_ascii_case("application/trickle-ice-sdpfrag")
            }
            _ => false,
        })
    }

    pub async fn run_server_events_loop(
        context: CallContext,
        proxy_config: Arc<crate::config::ProxyConfig>,
        dialog_layer: Arc<DialogLayer>,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        server_timer: Arc<Mutex<SessionTimerState>>,
        client_timer: Arc<Mutex<SessionTimerState>>,
        callee_dialogs: Arc<Mutex<HashSet<DialogId>>>,
        server_dialog: ServerInviteDialog,
        handle: CallSessionHandle,
        cancel_token: CancellationToken,
        pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
        _shared: CallSessionShared,
    ) -> Result<()> {
        let mut refresh_interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    debug!(session_id = %context.session_id, "Session loop cancelled via token");
                    break;
                }
                state = state_rx.recv() => {
                    match state {
                        Some(state) => {
                            match state {
                                DialogState::Terminated(dialog_id, reason) => {
                                    debug!(session_id = %context.session_id, reason = ?reason, %dialog_id, "Server dialog terminated");
                                    CallSession::store_pending_hangup(
                                        &pending_hangup,
                                        Some(CallRecordHangupReason::ByCaller),
                                        Some(200),
                                        Some("caller".to_string()),
                                    ).ok();
                                    cancel_token.cancel();
                                    break;
                                }
                                DialogState::Info(dialog_id, request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received INFO on server dialog");
                                    if Self::is_trickle_ice_info(&request) && !request.body.is_empty() {
                                        let payload = String::from_utf8_lossy(&request.body).to_string();
                                        let _ = handle.send_command(SessionAction::HandleTrickleIce(payload));
                                    }
                                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                }
                                DialogState::Updated(dialog_id, request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received UPDATE/INVITE on server dialog");

                                    let has_sdp = !request.body.is_empty();
                                    let is_reinvite = request.method == rsip::Method::Invite;
                                    let is_update = request.method == rsip::Method::Update;

                                    if (is_reinvite || is_update) && has_sdp {
                                        let sdp = String::from_utf8_lossy(&request.body).to_string();
                                        let _ = handle.send_command(SessionAction::HandleReInvite(request.method.to_string(), sdp));

                                        if is_update {
                                            tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                            debug!(session_id = %context.session_id, "Replied to UPDATE (200 OK)");
                                        }
                                    } else {
                                        SipLeg::refresh_timer_state(&server_timer, &request.headers);
                                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                    }
                                }
                                DialogState::Notify(dialog_id, _request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received NOTIFY on server dialog");
                                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                }
                                DialogState::Options(dialog_id, _request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received Option on server dialog");
                                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                }
                                DialogState::Calling(dialog_id) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in calling state");
                                }
                                DialogState::Trying(dialog_id) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in trying state");
                                }
                                DialogState::Early(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in early state");
                                }
                                DialogState::WaitAck(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in wait-ack state");
                                }
                                DialogState::Confirmed(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in confirmed state");
                                }
                                other_state => {
                                    debug!(
                                        session_id = %context.session_id,
                                        "Received other state on server dialog: {}", other_state
                                    );
                                }
                            }
                        }
                        None => {
                            warn!(session_id = %context.session_id, "Server dialog state channel closed");
                            break
                        }
                    }
                }
                state = callee_state_rx.recv() => {
                    match state {
                        Some(state) => {
                            match state {
                                DialogState::Terminated(dialog_id, reason) => {
                                    let is_active = {
                                        let dialogs = callee_dialogs.lock().unwrap();
                                        dialogs.contains(&dialog_id)
                                    };
                                    if is_active {
                                        info!(session_id = %context.session_id, reason = ?reason, %dialog_id, "Callee dialog terminated");
                                        CallSession::store_pending_hangup(
                                            &pending_hangup,
                                            Some(CallRecordHangupReason::ByCallee),
                                            Some(200),
                                            Some("callee".to_string()),
                                        ).ok();
                                        cancel_token.cancel();
                                        break;
                                    } else {
                                        debug!(session_id = %context.session_id, %dialog_id, "Inactive callee dialog terminated, ignoring");
                                    }
                                }
                                DialogState::Updated(dialog_id, _request, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received UPDATE/INVITE on callee dialog");
                                    {
                                        let mut timer = client_timer.lock().unwrap();
                                        if timer.active {
                                            timer.update_refresh();
                                            debug!(session_id = %context.session_id, "Client session timer refreshed by incoming request from callee");
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => {
                            warn!(session_id = %context.session_id, "Callee dialog state channel closed");
                            break;
                        }
                    }
                }
                _ = refresh_interval.tick() => {
                    if !proxy_config.session_timer {
                        continue;
                    }
                    let mut should_terminate = false;
                    let mut should_refresh_server = false;
                    let mut should_refresh_client = false;

                    {
                        let server_timer_guard = server_timer.lock().unwrap();
                        if server_timer_guard.is_expired() {
                            debug!(session_id = %context.session_id, "Server session timer expired, terminating");
                            should_terminate = true;
                        } else if server_timer_guard.refresher == SessionRefresher::Uas && server_timer_guard.should_refresh() {
                            should_refresh_server = true;
                        }
                    }

                    if !should_terminate {
                        let client_timer_guard = client_timer.lock().unwrap();
                        if client_timer_guard.is_expired() {
                            info!(session_id = %context.session_id, "Client session timer expired, terminating");
                            should_terminate = true;
                        } else if client_timer_guard.refresher == SessionRefresher::Uac && client_timer_guard.should_refresh() {
                            should_refresh_client = true;
                        }
                    }

                    if should_terminate {
                        info!(session_id = %context.session_id, "Session timer expired, cancelling call");
                        cancel_token.cancel();
                        break;
                    }

                    if should_refresh_server {
                        debug!(session_id = %context.session_id, "Server session timer: sending refresh (UAS)");

                        let session_interval = {
                            let mut timer = server_timer.lock().unwrap();
                            timer.refreshing = true;
                            timer.session_interval
                        };

                        let headers = vec![
                            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG).into()),
                            rsip::Header::Other(
                                HEADER_SESSION_EXPIRES.into(),
                                format!("{};refresher=uas", session_interval.as_secs()),
                            ),
                        ];

                        let server_dialog = server_dialog.clone();
                        let server_timer = server_timer.clone();
                        let session_id = context.session_id.clone();
                        let cancel_token_clone = cancel_token.clone();

                        crate::utils::spawn(async move {
                            tokio::select!{
                                _ = cancel_token_clone.cancelled() => {
                                    debug!(session_id = %session_id, "Not sending UPDATE for session refresh, session cancelled");
                                },
                                result = server_dialog.update(Some(headers), None) => {
                                    let mut timer = server_timer.lock().unwrap();
                                    timer.refreshing = false;
                                    match result {
                                        Err(e) => {
                                            warn!(session_id = %session_id, "Failed to send UPDATE for session refresh: {}", e);
                                        }
                                        Ok(None) => {
                                            warn!(session_id = %session_id, "UPDATE for session refresh returned no response");
                                        }
                                        Ok(Some(resp)) => {
                                            if matches!(resp.status_code.kind(), rsip::status_code::StatusCodeKind::Successful) {
                                                timer.update_refresh();
                                            } else {
                                                warn!(session_id = %session_id, status = %resp.status_code, "UPDATE for session refresh failed");
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }

                    if should_refresh_client {
                        debug!(session_id = %context.session_id, "Client session timer: sending refresh (UAC)");

                        let session_interval = {
                            let mut timer = client_timer.lock().unwrap();
                            timer.refreshing = true;
                            timer.session_interval
                        };

                        let headers = vec![
                            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG).into()),
                            rsip::Header::Other(
                                HEADER_SESSION_EXPIRES.into(),
                                format!("{};refresher=uac", session_interval.as_secs()),
                            ),
                        ];

                        let dialog_ids: Vec<DialogId> = {
                            let dialogs = callee_dialogs.lock().unwrap();
                            dialogs.iter().cloned().collect()
                        };

                        for dialog_id in dialog_ids {
                            if let Some(dialog) = dialog_layer.get_dialog(&dialog_id) {
                                match dialog {
                                    rsipstack::dialog::dialog::Dialog::ClientInvite(invite_dialog) => {
                                        let client_timer = client_timer.clone();
                                        let session_id = context.session_id.clone();
                                        let headers = headers.clone();
                                        let cancel_token_clone = cancel_token.clone();

                                        crate::utils::spawn(async move {
                                            tokio::select!{
                                                _ = cancel_token_clone.cancelled() => {
                                                    debug!(session_id = %session_id, %dialog_id, "Not sending UPDATE for session refresh, session cancelled");
                                                },
                                                result = invite_dialog.update(Some(headers), None) => {
                                                    let mut timer = client_timer.lock().unwrap();
                                                    timer.refreshing = false;
                                                    match result {
                                                        Err(e) => {
                                                            warn!(session_id = %session_id, %dialog_id, "Failed to send UPDATE for session refresh: {}", e);
                                                        }
                                                        Ok(None) => {
                                                            warn!(session_id = %session_id, %dialog_id, "UPDATE for session refresh returned no response");
                                                        }
                                                        Ok(Some(resp)) => {
                                                            if matches!(resp.status_code.kind(), rsip::status_code::StatusCodeKind::Successful) {
                                                                timer.update_refresh();
                                                            } else {
                                                                warn!(session_id = %session_id, %dialog_id, status = %resp.status_code, "UPDATE for session refresh failed");
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        });
                                    }
                                    _ => {
                                        let mut timer = client_timer.lock().unwrap();
                                        timer.refreshing = false;
                                        warn!(session_id = %context.session_id, %dialog_id, "Dialog is not a ClientInvite dialog, cannot send UPDATE");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
