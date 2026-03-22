use crate::proxy::proxy_call::leg_event::{LegEvent, TerminationReason};
use crate::proxy::proxy_call::session::{CallSession, PendingHangup};
use crate::proxy::proxy_call::session_timer::{
    HEADER_SESSION_EXPIRES, SessionRefresher, SessionTimerState, TIMER_TAG,
};
use crate::proxy::proxy_call::sip_leg::SipLeg;
use crate::proxy::proxy_call::state::{CallContext, CallSessionHandle, CallSessionShared, MidDialogLeg, SessionAction};
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

/// Task that processes exported-leg (server) dialog events and emits `LegEvent`s.
async fn run_exported_event_task(
    session_id: String,
    mut state_rx: mpsc::UnboundedReceiver<DialogState>,
    server_timer: Arc<Mutex<SessionTimerState>>,
    shared: CallSessionShared,
    leg_tx: mpsc::UnboundedSender<LegEvent>,
    cancel_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            state = state_rx.recv() => {
                match state {
                    Some(DialogState::Terminated(dialog_id, reason)) => {
                        debug!(session_id = %session_id, reason = ?reason, %dialog_id, "Server dialog terminated");
                        let _ = leg_tx.send(LegEvent::Terminated {
                            reason: TerminationReason::ByCaller,
                        });
                        break;
                    }
                    Some(DialogState::Info(dialog_id, request, tx_handle)) => {
                        debug!(session_id = %session_id, %dialog_id, "Received INFO on server dialog");
                        if is_trickle_ice_info(&request) && !request.body.is_empty() {
                            let payload = String::from_utf8_lossy(&request.body).to_string();
                            let _ = leg_tx.send(LegEvent::TrickleIce { payload });
                        }
                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                    }
                    Some(DialogState::Updated(dialog_id, request, tx_handle)) => {
                        debug!(session_id = %session_id, %dialog_id, "Received UPDATE/INVITE on server dialog");

                        let is_reinvite = request.method == rsip::Method::Invite;
                        let is_update = request.method == rsip::Method::Update;

                        if is_reinvite || is_update {
                            let has_sdp = !request.body.is_empty();
                            let sdp = has_sdp.then(|| String::from_utf8_lossy(&request.body).to_string());
                            shared.store_mid_dialog_reply(MidDialogLeg::Caller, &dialog_id.to_string(), tx_handle);
                            let _ = leg_tx.send(LegEvent::ReInvite {
                                leg: MidDialogLeg::Caller,
                                method: request.method,
                                sdp,
                                dialog_id: dialog_id.to_string(),
                            });
                        } else {
                            SipLeg::refresh_timer_state(&server_timer, &request.headers);
                            tx_handle.reply(rsip::StatusCode::OK).await.ok();
                        }
                    }
                    Some(DialogState::Notify(dialog_id, _request, tx_handle)) => {
                        debug!(session_id = %session_id, %dialog_id, "Received NOTIFY on server dialog");
                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                    }
                    Some(DialogState::Options(dialog_id, _request, tx_handle)) => {
                        debug!(session_id = %session_id, %dialog_id, "Received Option on server dialog");
                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                    }
                    Some(DialogState::Calling(dialog_id)) => {
                        debug!(session_id = %session_id, %dialog_id, "Server dialog in calling state");
                    }
                    Some(DialogState::Trying(dialog_id)) => {
                        debug!(session_id = %session_id, %dialog_id, "Server dialog in trying state");
                    }
                    Some(DialogState::Early(dialog_id, _)) => {
                        debug!(session_id = %session_id, %dialog_id, "Server dialog in early state");
                    }
                    Some(DialogState::WaitAck(dialog_id, _)) => {
                        debug!(session_id = %session_id, %dialog_id, "Server dialog in wait-ack state");
                    }
                    Some(DialogState::Confirmed(dialog_id, _)) => {
                        debug!(session_id = %session_id, %dialog_id, "Server dialog in confirmed state");
                    }
                    Some(other_state) => {
                        debug!(
                            session_id = %session_id,
                            "Received other state on server dialog: {}", other_state
                        );
                    }
                    None => {
                        warn!(session_id = %session_id, "Server dialog state channel closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Task that processes target-leg (client) dialog events and emits `LegEvent`s.
async fn run_target_event_task(
    session_id: String,
    mut target_state_rx: mpsc::UnboundedReceiver<DialogState>,
    client_timer: Arc<Mutex<SessionTimerState>>,
    target_dialogs: Arc<Mutex<HashSet<DialogId>>>,
    shared: CallSessionShared,
    leg_tx: mpsc::UnboundedSender<LegEvent>,
    cancel_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            state = target_state_rx.recv() => {
                match state {
                    Some(DialogState::Terminated(dialog_id, reason)) => {
                        let is_active = {
                            let mut dialogs = target_dialogs.lock().unwrap();
                            dialogs.remove(&dialog_id)
                        };
                        if is_active {
                            info!(session_id = %session_id, reason = ?reason, %dialog_id, "Callee dialog terminated");
                            let _ = leg_tx.send(LegEvent::Terminated {
                                reason: TerminationReason::ByCallee,
                            });
                            break;
                        } else {
                            debug!(session_id = %session_id, %dialog_id, "Inactive callee dialog terminated, ignoring");
                        }
                    }
                    Some(DialogState::Updated(dialog_id, request, tx_handle)) => {
                        debug!(session_id = %session_id, %dialog_id, "Received UPDATE/INVITE on callee dialog");
                        {
                            let mut timer = client_timer.lock().unwrap();
                            if timer.active {
                                timer.update_refresh();
                                debug!(session_id = %session_id, "Client session timer refreshed by incoming request from callee");
                            }
                        }
                        let is_reinvite = request.method == rsip::Method::Invite;
                        let is_update = request.method == rsip::Method::Update;
                        if is_reinvite || is_update {
                            let has_sdp = !request.body.is_empty();
                            let sdp = has_sdp.then(|| String::from_utf8_lossy(&request.body).to_string());
                            shared.store_mid_dialog_reply(MidDialogLeg::Callee, &dialog_id.to_string(), tx_handle);
                            let _ = leg_tx.send(LegEvent::ReInvite {
                                leg: MidDialogLeg::Callee,
                                method: request.method,
                                sdp,
                                dialog_id: dialog_id.to_string(),
                            });
                        }
                    }
                    Some(_) => {}
                    None => {
                        warn!(session_id = %session_id, "Callee dialog state channel closed");
                        break;
                    }
                }
            }
        }
    }
}

impl SessionLoopRuntime {
    pub async fn run_server_events_loop(
        context: CallContext,
        proxy_config: Arc<crate::config::ProxyConfig>,
        dialog_layer: Arc<DialogLayer>,
        state_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
        target_state_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
        server_timer: Arc<Mutex<SessionTimerState>>,
        client_timer: Arc<Mutex<SessionTimerState>>,
        target_dialogs: Arc<Mutex<HashSet<DialogId>>>,
        server_dialog: Option<ServerInviteDialog>,  // exported leg's server dialog clone for timer refresh (None for non-proxy sessions)
        handle: CallSessionHandle,
        cancel_token: CancellationToken,
        pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
        shared: CallSessionShared,
    ) -> Result<()> {
        let (leg_tx, mut leg_rx) = mpsc::unbounded_channel::<LegEvent>();

        // Spawn exported-leg event task only when an inbound server dialog exists (proxy sessions)
        if let Some(state_rx) = state_rx {
            crate::utils::spawn(run_exported_event_task(
                context.session_id.clone(),
                state_rx,
                server_timer.clone(),
                shared.clone(),
                leg_tx.clone(),
                cancel_token.child_token(),
            ));
        }
        if let Some(target_rx) = target_state_rx {
            crate::utils::spawn(run_target_event_task(
                context.session_id.clone(),
                target_rx,
                client_timer.clone(),
                target_dialogs.clone(),
                shared,
                leg_tx,
                cancel_token.child_token(),
            ));
        }

        let mut refresh_interval = tokio::time::interval(Duration::from_secs(5));

        // Termination sequencing: when first leg terminates, wait briefly
        // for the other leg before cancelling the session token.
        let mut terminating = false;
        let termination_deadline = tokio::time::sleep(Duration::from_secs(86400));
        tokio::pin!(termination_deadline);

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    debug!(session_id = %context.session_id, "Session loop cancelled via token");
                    break;
                }
                _ = &mut termination_deadline, if terminating => {
                    debug!(session_id = %context.session_id, "Termination grace period expired, cancelling session");
                    cancel_token.cancel();
                    break;
                }
                event = leg_rx.recv() => {
                    match event {
                        Some(LegEvent::Terminated { reason }) => {
                            let (hangup_reason, code, initiator) = match reason {
                                TerminationReason::ByCaller => {
                                    debug!(session_id = %context.session_id, "Caller leg terminated");
                                    (CallRecordHangupReason::ByCaller, 200, "caller")
                                }
                                TerminationReason::ByCallee => {
                                    info!(session_id = %context.session_id, "Callee leg terminated");
                                    (CallRecordHangupReason::ByCallee, 200, "callee")
                                }
                            };
                            if !terminating {
                                // First leg terminated: store hangup reason, start grace period
                                CallSession::store_pending_hangup(
                                    &pending_hangup,
                                    Some(hangup_reason),
                                    Some(code),
                                    Some(initiator.to_string()),
                                ).ok();
                                terminating = true;
                                termination_deadline.as_mut().reset(
                                    tokio::time::Instant::now() + Duration::from_secs(5),
                                );
                                debug!(
                                    session_id = %context.session_id,
                                    initiator,
                                    "Entering termination grace period, waiting for other leg"
                                );
                            } else {
                                // Second leg terminated: clean exit
                                debug!(
                                    session_id = %context.session_id,
                                    initiator,
                                    "Both legs terminated"
                                );
                                cancel_token.cancel();
                                break;
                            }
                        }
                        Some(LegEvent::ReInvite { leg, method, sdp, dialog_id }) => {
                            if !terminating {
                                let _ = handle.send_command(SessionAction::HandleReInvite {
                                    leg,
                                    method: method.to_string(),
                                    sdp,
                                    dialog_id,
                                });
                            }
                        }
                        Some(LegEvent::TrickleIce { payload }) => {
                            if !terminating {
                                let _ = handle.send_command(SessionAction::HandleTrickleIce(payload));
                            }
                        }
                        None => {
                            debug!(session_id = %context.session_id, "All leg event senders dropped");
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
                        if let Some(ref server_dialog) = server_dialog {
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
                        } else {
                            debug!(session_id = %context.session_id, "Server session timer refresh skipped: no server dialog");
                        }
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
                            let dialogs = target_dialogs.lock().unwrap();
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
