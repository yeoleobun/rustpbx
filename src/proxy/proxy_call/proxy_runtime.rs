use crate::call::sip::ServerDialogGuard;
use crate::call::{CallForwardingMode, DialStrategy, Location, TransferEndpoint};
use crate::callrecord::CallRecordSender;
use crate::config::RouteResult;
use crate::proxy::proxy_call::media_peer::VoiceEnginePeer;
use crate::proxy::proxy_call::queue_flow::QueueFlow;
use crate::proxy::proxy_call::reporter::CallReporter;
use crate::proxy::proxy_call::session::{ActionInbox, CallSession, SessionActionInbox};
use crate::proxy::proxy_call::sip_leg::SipLeg;
use crate::proxy::proxy_call::state::{CallContext, CallSessionHandle, CallSessionShared};
use crate::proxy::proxy_call::target_runtime::TargetRuntime;
use crate::proxy::routing::matcher::RouteResourceLookup;
use crate::proxy::server::SipServerRef;
use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::BoxFuture;
use rsip::StatusCode;
use rsip::Uri;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Proxy-mode runtime helper. Contains orchestration logic specific to
/// B2BUA proxied calls: target dialing, transfer, hold/unhold.
///
/// `CallSession` remains the single owner — this is an internal helper
/// that `CallSession` delegates to, not a second owner.
pub(crate) struct ProxySessionRuntime;

impl ProxySessionRuntime {
    // ── Bootstrap ────────────────────────────────────────────────────

    /// Proxy-mode session bootstrap: validate SDP, create server dialog,
    /// build session, and spawn the process task with server dialog event loop.
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
        let use_media_proxy = CallSession::check_media_proxy(
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

    // ── Target orchestration ──────────────────────────────────────────

    /// Run targets according to the dial strategy (sequential or parallel).
    pub async fn run_targets(
        session: &mut CallSession,
        strategy: &DialStrategy,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        session.shared.transition_to_dialing();
        session.process_pending_actions(inbox.as_deref_mut()).await?;
        debug!(
            session_id = %session.context.session_id,
            strategy = %strategy,
            media_proxy = session.use_media_proxy,
            "executing dialplan"
        );

        match strategy {
            DialStrategy::Sequential(targets) => {
                TargetRuntime::dial_sequential(session, targets, inbox).await
            }
            DialStrategy::Parallel(targets) => {
                TargetRuntime::dial_parallel(session, targets, inbox).await
            }
        }
    }

    /// Prepare and send INVITE to a single proxy target.
    /// Handles routing function invocation, same-realm user lookup, and contact enforcement.
    pub async fn try_single_target(
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

        // Generate offer based on target's WebRTC support
        let offer = session.create_offer_for_target(target).await;

        let enforced_contact = session.local_contact_uri();
        let headers = session
            .context
            .dialplan
            .build_invite_headers(&target)
            .unwrap_or_default();
        let invite_option = session.target_leg().sip.build_outbound_invite_option(
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

        let mut invite_option =
            if let Some(ref route_invite) = session.context.dialplan.route_invite {
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
                        debug!(session_id = session.context.session_id, %target,
                            "Routing function returned NotHandled"
                        );
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
        if session.server.is_same_realm(&callee_realm).await {
            let dialplan = &session.context.dialplan;
            let locations = session
                .server
                .locator
                .lookup(&callee_uri)
                .await
                .map_err(|e| {
                    (
                        rsip::StatusCode::TemporarilyUnavailable,
                        Some(e.to_string()),
                    )
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
                        return Err((
                            rsip::StatusCode::ServerInternalError,
                            Some(e.to_string()),
                        ));
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

        TargetRuntime::try_single_target(session, target).await
    }

    // ── Transfer ──────────────────────────────────────────────────────

    /// Handle proxy-mode transfer: parse endpoint type, delegate to URI or queue.
    pub fn transfer_target<'a>(
        session: &'a mut CallSession,
        target: &'a str,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            if let Some(endpoint) = TransferEndpoint::parse(target) {
                Self::transfer_to_endpoint(session, &endpoint, inbox).await
            } else {
                Self::transfer_to_uri(session, target).await
            }
        }
        .boxed()
    }

    /// Transfer to an endpoint (URI or queue).
    pub fn transfer_to_endpoint<'a>(
        session: &'a mut CallSession,
        endpoint: &'a TransferEndpoint,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match endpoint {
                TransferEndpoint::Uri(uri) => Self::transfer_to_uri(session, uri).await,
                TransferEndpoint::Queue(name) => {
                    if let Some(inbox) = inbox {
                        info!(session_id = %session.context.session_id, queue = %name, "Transferring to queue");

                        let lookup_ref = if name.chars().all(|c| c.is_ascii_digit()) {
                            format!("db-{}", name)
                        } else {
                            name.clone()
                        };

                        let queue_config = session
                            .server
                            .data_context
                            .load_queue(&lookup_ref)
                            .await?
                            .ok_or_else(|| anyhow!("Queue not found: {}", name))?;

                        let mut plan = queue_config.to_queue_plan()?;
                        if plan.label.is_none() {
                            plan.label = Some(name.clone());
                        }

                        QueueFlow::execute_queue_plan(session, &plan, None, Some(inbox)).await
                    } else {
                        warn!("Queue forwarding not supported without inbox: {}", name);
                        Err(anyhow!("Queue forwarding not supported without inbox"))
                    }
                }
            }
        }
        .boxed()
    }

    /// Transfer to a URI target.
    async fn transfer_to_uri(session: &mut CallSession, uri: &str) -> Result<()> {
        let parsed = Uri::try_from(uri)
            .map_err(|err| anyhow!("invalid forwarding uri '{}': {}", uri, err))?;
        let mut location = Location::default();
        location.aor = parsed.clone();
        location.contact_raw = Some(parsed.to_string());
        match Self::try_single_target(session, &location).await {
            Ok(_) => Ok(()),
            Err((code, reason)) => {
                let message = reason.unwrap_or_else(|| code.to_string());
                Err(anyhow!("forwarding to {} failed: {}", uri, message))
            }
        }
    }

    // ── Hold / Unhold ─────────────────────────────────────────────────

    /// Proxy-mode hold: play hold music to the exported leg.
    pub async fn hold(session: &mut CallSession, music_source: Option<&str>) -> Result<()> {
        let audio_file = music_source.unwrap_or_default();
        if !audio_file.is_empty() {
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

    /// Proxy-mode unhold: stop hold on the exported leg.
    pub async fn unhold(session: &mut CallSession) -> Result<()> {
        session.exported_leg.unhold().await;
        Ok(())
    }

    /// Handle proxy-mode failure: busy/no-answer forwarding, voicemail fallback,
    /// or final failure reporting.
    pub async fn handle_failure(session: &mut CallSession, inbox: ActionInbox<'_>) -> Result<()> {
        if session.failure_is_busy() {
            if let Some(config) = session.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenBusy) {
                    info!(
                        session_id = %session.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (busy) engaged"
                    );
                    return Self::transfer_to_endpoint(session, &config.endpoint, inbox).await;
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
                    return Self::transfer_to_endpoint(session, &config.endpoint, inbox).await;
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
