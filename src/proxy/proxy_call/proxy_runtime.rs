use crate::call::{DialStrategy, Location, TransferEndpoint};
use crate::config::RouteResult;
use crate::proxy::proxy_call::queue_flow::QueueFlow;
use crate::proxy::proxy_call::session::{ActionInbox, CallSession};
use crate::proxy::proxy_call::target_runtime::TargetRuntime;
use crate::proxy::routing::matcher::RouteResourceLookup;
use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::BoxFuture;
use rsip::StatusCode;
use rsip::Uri;
use tracing::{debug, info, warn};

/// Proxy-mode runtime helper. Contains orchestration logic specific to
/// B2BUA proxied calls: target dialing, transfer, hold/unhold.
///
/// `CallSession` remains the single owner — this is an internal helper
/// that `CallSession` delegates to, not a second owner.
pub(crate) struct ProxySessionRuntime;

impl ProxySessionRuntime {
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

    /// Handle proxy-mode failure by delegating to TargetRuntime.
    pub async fn handle_failure(session: &mut CallSession, inbox: ActionInbox<'_>) -> Result<()> {
        TargetRuntime::handle_failure(session, inbox).await
    }
}
