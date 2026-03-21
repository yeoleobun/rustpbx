use crate::call::{CallForwardingMode, Location};
use crate::config::RouteResult;
use crate::proxy::proxy_call::session::{ActionInbox, CallSession, ParallelEvent};
use crate::proxy::proxy_call::state::SessionAction;
use anyhow::{Result, anyhow};
use futures::FutureExt;
use rsip::StatusCode;
use rsipstack::dialog::{DialogId, dialog::DialogState, invitation::InviteOption};
use rsipstack::rsip_ext::RsipResponseExt;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{debug, info, warn};

use crate::proxy::proxy_call::session_timer::{
    HEADER_MIN_SE, HEADER_SESSION_EXPIRES, get_header_value, parse_min_se,
};
use crate::call::sip::DialogStateReceiverGuard;

pub(crate) struct TargetRuntime;

impl TargetRuntime {
    pub(crate) async fn dial_sequential(
        session: &mut CallSession,
        targets: &[Location],
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        debug!(
            session_id = %session.context.session_id,
            target_count = targets.len(),
            "Starting sequential dialing"
        );

        for (index, target) in targets.iter().enumerate() {
            session.process_pending_actions(inbox.as_deref_mut()).await?;
            debug!(
                session_id = %session.context.session_id, index, %target,
                "trying sequential target"
            );

            let result = crate::proxy::proxy_call::queue_flow::QueueFlow::run_target_attempt(session, target).await;

            match result {
                Ok(_) => {
                    debug!(
                        session_id = %session.context.session_id,
                        target_index = index,
                        "Sequential target succeeded"
                    );
                    return Ok(());
                }
                Err((code, reason)) => {
                    info!(
                        session_id = %session.context.session_id,
                        target_index = index,
                        code = %code,
                        reason,
                        "Sequential target failed"
                    );

                    let should_retry = crate::proxy::proxy_call::queue_flow::QueueFlow::should_retry_code(
                        session.get_retry_codes(),
                        code.clone(),
                    );
                    if should_retry {
                        info!(
                            session_id = %session.context.session_id,
                            code = %code,
                            "Code is in retry list, proceeding to next target"
                        );
                    } else {
                        info!(
                            session_id = %session.context.session_id,
                            code = %code,
                            "Code is NOT in retry list, stopping failover"
                        );
                    }

                    session.note_attempt_failure(
                        code.clone(),
                        reason.clone(),
                        Some(target.aor.to_string()),
                    );
                    session.set_error(code, reason, Some(target.aor.to_string()));

                    if should_retry {
                        continue;
                    } else {
                        return Err(anyhow!("Target failed and retry policy stopped failover"));
                    }
                }
            }
        }

        Err(anyhow!("All sequential targets failed"))
    }

    pub(crate) async fn dial_parallel(
        session: &mut CallSession,
        targets: &[Location],
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(
            session_id = %session.context.session_id,
            target_count = targets.len(),
            "Starting parallel dialing"
        );

        if targets.is_empty() {
            session.set_error(
                StatusCode::ServerInternalError,
                Some("No targets provided".to_string()),
                None,
            );
            return Err(anyhow!("No targets provided for parallel dialing"));
        }

        if !session.caller_leg.media.early_media_sent {
            let _ = session
                .apply_session_action(
                    SessionAction::StartRinging {
                        ringback: None,
                        passthrough: false,
                    },
                    None,
                )
                .await;
        }
        session.process_pending_actions(inbox.as_deref_mut()).await?;

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<ParallelEvent>();
        let cancel_token = session.cancel_token.clone();

        let caller = match session.context.dialplan.caller.as_ref() {
            Some(c) => c.clone(),
            None => {
                session.set_error(
                    StatusCode::ServerInternalError,
                    Some("No caller specified in dialplan".to_string()),
                    None,
                );
                return Err(anyhow!("No caller specified in dialplan"));
            }
        };

        let local_contact = session.local_contact_uri();
        let mut join_set = JoinSet::<()>::new();

        for (idx, target) in targets.iter().enumerate() {
            let (state_tx, state_rx) = mpsc::unbounded_channel::<DialogState>();
            let ev_tx_c = ev_tx.clone();
            let offer = session.create_offer_for_target(target).await;
            let headers = session
                .context
                .dialplan
                .build_invite_headers(target)
                .unwrap_or_default();
            let invite_option = session.callee_leg.sip.build_outbound_invite_option(
                target,
                caller.clone(),
                None,
                offer,
                local_contact.clone(),
                headers,
                session.context.max_forwards,
                None,
                None,
            );

            let invite_caller = invite_option.caller.to_string();
            let invite_callee = invite_option.callee.to_string();
            let invite_contact = invite_option.contact.to_string();
            let invite_destination = invite_option
                .destination
                .as_ref()
                .map(|addr| addr.to_string());
            let aor = target.aor.to_string();

            let callee_event_tx = session.callee_leg.sip.dialog_event_tx.clone();
            let dialog_layer_for_guard = session.dialog_layer.clone();
            let dialog_layer_for_invite = session.dialog_layer.clone();

            crate::utils::spawn({
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    let mut guard = DialogStateReceiverGuard::new(dialog_layer_for_guard, state_rx);
                    while let Some(state) = guard.recv().await {
                        if let Some(tx) = &callee_event_tx {
                            let _ = tx.send(state.clone());
                        }
                        match state {
                            DialogState::Calling(dialog_id) => {
                                guard.set_dialog_id(dialog_id.clone());
                                let _ = ev_tx_c.send(ParallelEvent::Calling {
                                    _idx: idx,
                                    dialog_id,
                                });
                            }
                            DialogState::Early(dialog_id, response) => {
                                guard.set_dialog_id(dialog_id.clone());
                                let sdp = if !response.body().is_empty() {
                                    Some(String::from_utf8_lossy(response.body()).to_string())
                                } else {
                                    None
                                };
                                let _ = ev_tx_c.send(ParallelEvent::Early {
                                    _idx: idx,
                                    dialog_id,
                                    sdp,
                                });
                            }
                            DialogState::Terminated(_, _) => {
                                let _ = ev_tx_c.send(ParallelEvent::Terminated { _idx: idx });
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            });

            let state_tx_c = state_tx.clone();

            join_set.spawn({
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    let invite_result = dialog_layer_for_invite
                        .do_invite(invite_option, state_tx_c)
                        .await;
                    match invite_result {
                        Ok((dialog, resp_opt)) => {
                            if let Some(resp) = resp_opt {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let answer = String::from_utf8_lossy(resp.body()).to_string();
                                    let _ = ev_tx_c.send(ParallelEvent::Accepted {
                                        _idx: idx,
                                        dialog_id: dialog.id(),
                                        answer,
                                        aor,
                                        caller_uri: invite_caller,
                                        callee_uri: invite_callee,
                                        contact: invite_contact,
                                        destination: invite_destination,
                                    });
                                } else {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    let _ = ev_tx_c.send(ParallelEvent::Failed {
                                        _idx: idx,
                                        code: resp.status_code,
                                        reason,
                                        target: Some(invite_callee.clone()),
                                    });
                                }
                            } else {
                                let _ = ev_tx_c.send(ParallelEvent::Failed {
                                    _idx: idx,
                                    code: StatusCode::RequestTerminated,
                                    reason: Some("Cancelled by callee".to_string()),
                                    target: Some(invite_callee.clone()),
                                });
                            }
                        }
                        Err(e) => {
                            let (code, reason) = match e {
                                rsipstack::Error::DialogError(reason, _, code) => {
                                    (code, Some(reason))
                                }
                                _ => (
                                    StatusCode::ServerInternalError,
                                    Some("Invite failed".to_string()),
                                ),
                            };
                            let _ = ev_tx_c.send(ParallelEvent::Failed {
                                _idx: idx,
                                code,
                                reason,
                                target: Some(invite_callee.clone()),
                            });
                        }
                    }
                }
            });
        }

        {
            let ev_tx_c = ev_tx.clone();
            let cancel_token = cancel_token.clone();
            join_set.spawn(async move {
                cancel_token.cancelled().await;
                let _ = ev_tx_c.send(ParallelEvent::Cancelled);
            });
        }

        drop(ev_tx);

        let mut failures = 0usize;
        let mut accepted_idx: Option<usize> = None;
        let mut known_dialogs: Vec<Option<DialogId>> = vec![None; targets.len()];

        loop {
            tokio::select! {
                maybe_event = ev_rx.recv() => {
                    let event = match maybe_event {
                        Some(e) => e,
                        None => break,
                    };

                    match event {
                        ParallelEvent::Calling { _idx: idx, dialog_id } => {
                            known_dialogs[idx] = Some(dialog_id);
                        }
                        ParallelEvent::Early { _idx: _, dialog_id, sdp } => {
                            if let Some(answer) = sdp {
                                let _ = session
                                    .apply_session_action(
                                        SessionAction::ProvideEarlyMediaWithDialog(
                                            dialog_id.to_string(),
                                            answer,
                                        ),
                                        None,
                                    )
                                    .await;
                            } else if !session.caller_leg.media.early_media_sent {
                                let _ = session
                                    .apply_session_action(
                                        SessionAction::StartRinging {
                                            ringback: None,
                                            passthrough: false,
                                        },
                                        None,
                                    )
                                    .await;
                            }
                        }
                        ParallelEvent::Accepted {
                            _idx: idx,
                            dialog_id,
                            answer,
                            aor,
                            caller_uri,
                            callee_uri,
                            contact,
                            destination,
                        } => {
                            session.routed_caller = Some(caller_uri.clone());
                            session.routed_callee = Some(callee_uri.clone());
                            session.routed_contact = Some(contact.clone());
                            session.routed_destination = destination.clone();
                            if accepted_idx.is_none() {
                                if let Err(e) = session
                                    .apply_session_action(
                                        SessionAction::AcceptCall {
                                            callee: Some(aor),
                                            sdp: Some(answer),
                                            dialog_id: Some(dialog_id.to_string()),
                                        },
                                        None,
                                    )
                                    .await
                                {
                                    warn!(session_id = %session.context.session_id, error = %e, "Failed to accept call on parallel branch");
                                    continue;
                                }
                                accepted_idx = Some(idx);
                                session.add_callee_dialog(dialog_id.clone());
                                for (j, maybe_id) in known_dialogs.iter().enumerate() {
                                    if j != idx {
                                        if let Some(id) = maybe_id.clone() {
                                            if let Some(dlg) = session.dialog_layer.get_dialog(&id) {
                                                dlg.hangup().await.ok();
                                                session.dialog_layer.remove_dialog(&id);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ParallelEvent::Failed { code, reason, target, .. } => {
                            failures += 1;
                            session.set_error(code, reason, target);
                            if failures >= targets.len() && accepted_idx.is_none() {
                                return Err(anyhow!("All parallel targets failed"));
                            }
                        }
                        ParallelEvent::Terminated { _idx: idx } => {
                            if Some(idx) == accepted_idx {
                                return Ok(());
                            }
                        }
                        ParallelEvent::Cancelled => {
                            for maybe_id in known_dialogs.iter().filter_map(|o| o.as_ref()) {
                                if let Some(dlg) = session.dialog_layer.get_dialog(maybe_id) {
                                    dlg.hangup().await.ok();
                                    session.dialog_layer.remove_dialog(maybe_id);
                                }
                            }
                            return Err(anyhow!("Caller cancelled"));
                        }
                    }
                }
                maybe_action = async {
                    if let Some(ref mut inbox) = inbox {
                        inbox.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if let Some(action) = maybe_action {
                        session.apply_session_action(action, inbox.as_deref_mut()).await?;
                    }
                }
            }
        }

        if accepted_idx.is_some() {
            Ok(())
        } else {
            Err(anyhow!("Parallel dialing concluded without success"))
        }
    }

    pub(crate) async fn try_single_target(
        session: &mut CallSession,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let caller = session
            .context
            .dialplan
            .caller
            .as_ref()
            .ok_or((
                StatusCode::ServerInternalError,
                Some("No caller specified in dialplan".to_string()),
            ))?
            .clone();
        let caller_display_name = session.context.dialplan.caller_display_name.clone();
        let offer = session.create_offer_for_target(target).await;
        let enforced_contact = session.local_contact_uri();
        let headers = session
            .context
            .dialplan
            .build_invite_headers(target)
            .unwrap_or_default();
        let invite_option = session.callee_leg.sip.build_outbound_invite_option(
            target,
            caller.clone(),
            caller_display_name,
            offer,
            enforced_contact.clone(),
            headers,
            session.context.max_forwards,
            if session.server.proxy_config.session_timer {
                session.server.proxy_config.session_expires
            } else {
                None
            },
            session.context.dialplan.call_id.clone(),
        );

        let mut invite_option = if let Some(ref route_invite) = session.context.dialplan.route_invite {
            let route_result = route_invite
                .route_invite(
                    invite_option,
                    &session.context.dialplan.original,
                    &session.context.dialplan.direction,
                    &session.context.cookie,
                )
                .await
                .map_err(|e| {
                    warn!(session_id = %session.context.session_id, error = %e, "Routing function error");
                    (
                        StatusCode::ServerInternalError,
                        Some("Routing function error".to_string()),
                    )
                })?;
            match route_result {
                RouteResult::NotHandled(option, _) => {
                    info!(session_id = session.context.session_id, %target,
                        "Routing function returned NotHandled");
                    option
                }
                RouteResult::Forward(option, _) | RouteResult::Queue { option, .. } => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = session.context.session_id, %code, ?reason, "route abort");
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
        if invite_option.destination.is_none() && session.server.is_same_realm(&callee_realm).await {
            let dialplan = &session.context.dialplan;
            let locations = session.server.locator.lookup(callee_uri).await.map_err(|e| {
                (rsip::StatusCode::TemporarilyUnavailable, Some(e.to_string()))
            })?;

            if locations.is_empty() {
                match session
                    .server
                    .user_backend
                    .get_user(
                        &callee_uri.user().unwrap_or_default(),
                        Some(&callee_realm),
                        Some(&session.context.dialplan.original),
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
            session_id = %session.context.session_id, %caller, %target, destination,
            "Sending INVITE to callee"
        );

        Self::execute_invite(session, invite_option, target).await
    }

    async fn execute_invite(
        session: &mut CallSession,
        mut invite_option: InviteOption,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let _state_tx_keepalive = state_tx.clone();
        let dialog_layer = session.dialog_layer.clone();
        session.note_invite_details(&invite_option);

        info!(
            session_id = %session.context.session_id,
            callee = %invite_option.callee,
            destination = ?invite_option.destination,
            "Sending client INVITE to target"
        );

        let session_id = session.context.session_id.clone();
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
                                        let min_se_value = get_header_value(&resp.headers, HEADER_MIN_SE);
                                        if let Some(value) = min_se_value {
                                            if let Some(min_se) = parse_min_se(&value) {
                                                info!(
                                                    session_id = %session_id,
                                                    min_se = ?min_se,
                                                    "Received 422, retrying with new Session-Expires"
                                                );
                                                if let Some(headers) = &mut invite_option.headers {
                                                    headers.retain(|h| match h {
                                                        rsip::Header::Other(n, _) => !n.eq_ignore_ascii_case(HEADER_SESSION_EXPIRES),
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
                                return Err((StatusCode::RequestTerminated, Some("Cancelled by callee".to_string())));
                            }
                        }
                        Err(e) => {
                            debug!(session_id = %session_id, "Invite failed: {:?}", e);
                            return Err(match e {
                                rsipstack::Error::DialogError(reason, _, code) => (code, Some(reason)),
                                _ => (StatusCode::ServerInternalError, Some("Invite failed".to_string())),
                            });
                        }
                    }
                }
                state = state_rx.recv() => {
                    if let Some(DialogState::Early(dialog_id, response)) = state {
                        session.add_callee_dialog(dialog_id);
                        let sdp = String::from_utf8_lossy(response.body()).to_string();
                        let action = SessionAction::StartRinging{ ringback: Some(sdp), passthrough: false };
                        session.apply_session_action(action, None).await.ok();
                    }
                }
            }
        }

        let mut state_rx_guard = DialogStateReceiverGuard::new(dialog_layer.clone(), state_rx);
        state_rx_guard.set_dialog_id(dialog.id());

        if session.server.proxy_config.session_timer {
            let default_expires = session.server.proxy_config.session_expires.unwrap_or(1800);
            session.init_client_timer(&response, default_expires);
        }

        if response.status_code.kind() == rsip::StatusCodeKind::Successful {
            let answer_body = response.body();
            let sdp = if !answer_body.is_empty() {
                Some(String::from_utf8_lossy(answer_body).to_string())
            } else {
                None
            };

            let dialog_id_val = dialog.id();

            info!(
                session_id = %session.context.session_id,
                dialog_id = %dialog_id_val,
                "Client dialog established after 200 OK from callee"
            );

            session.add_callee_dialog(dialog_id_val.clone());
            let _ = session
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

        session.add_callee_guard(state_rx_guard);
        Ok(())
    }

    pub(crate) async fn handle_failure(session: &mut CallSession, inbox: ActionInbox<'_>) -> Result<()> {
        if session.failure_is_busy() {
            if let Some(config) = session.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenBusy) {
                    info!(
                        session_id = %session.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (busy) engaged"
                    );
                    return session.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
            if session.context.dialplan.voicemail_enabled {
                info!(
                    session_id = %session.context.session_id,
                    callee = %session.context.original_callee,
                    "busy: routing to voicemail"
                );
                let params = serde_json::json!({
                    "extension": session.context.original_callee,
                    "caller_id": session.context.original_caller,
                });
                return session.run_application("voicemail", Some(params), true).await;
            }
        } else if session.failure_is_no_answer() {
            if let Some(config) = session.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenNoAnswer) {
                    info!(
                        session_id = %session.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (no answer) engaged"
                    );
                    return session.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
            if session.context.dialplan.voicemail_enabled {
                info!(
                    session_id = %session.context.session_id,
                    callee = %session.context.original_callee,
                    "no-answer: routing to voicemail"
                );
                let params = serde_json::json!({
                    "extension": session.context.original_callee,
                    "caller_id": session.context.original_caller,
                });
                return session.run_application("voicemail", Some(params), true).await;
            }
        }

        if let Some((code, reason)) = session.last_error.clone() {
            session.report_failure(code, reason)?;
        } else {
            session.report_failure(
                StatusCode::ServerInternalError,
                Some("Unknown error".to_string()),
            )?;
        }
        Err(anyhow!("Call failed"))
    }
}
