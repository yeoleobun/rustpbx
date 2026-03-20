use crate::call::{DialStrategy, DialplanFlow, Location, QueueHoldConfig, QueuePlan};
use crate::proxy::proxy_call::caller_negotiation;
use crate::proxy::proxy_call::session::{ActionInbox, CallSession};
use anyhow::{Result, anyhow};
use rsip::StatusCode;
use tracing::{debug, info, warn};

pub(crate) struct QueueFlow;

impl QueueFlow {
    pub async fn execute_queue_plan(
        session: &mut CallSession,
        plan: &QueuePlan,
        next: Option<&DialplanFlow>,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        session.process_pending_actions(inbox.as_deref_mut()).await?;

        info!(
            session_id = %session.context.session_id,
            queue_label = ?plan.label,
            accept_immediately = plan.accept_immediately,
            "Executing queue plan"
        );

        let targets = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(targets)) | Some(DialStrategy::Parallel(targets)) => {
                targets.clone()
            }
            None => {
                warn!(session_id = %session.context.session_id, "No dial strategy in queue plan");
                return Self::handle_queue_failure(session, next, inbox).await;
            }
        };

        if targets.is_empty() {
            warn!(session_id = %session.context.session_id, "No targets in queue plan");
            return Self::handle_queue_failure(session, next, inbox).await;
        }

        if plan.accept_immediately {
            if let Err(e) = Self::prepare_immediate_accept(session).await {
                warn!(
                    session_id = %session.context.session_id,
                    error = %e,
                    "Failed to accept call for queue"
                );
                return Self::handle_queue_failure(session, next, inbox).await;
            }

            if let Some(hold_config) = &plan.hold {
                Self::start_hold(session, hold_config).await;
            }
        }

        let dial_result = match &plan.dial_strategy {
            Some(strategy) => session.run_targets(strategy, inbox.as_deref_mut()).await,
            None => {
                warn!(session_id = %session.context.session_id, "No dial strategy");
                Err(anyhow!("No dial strategy configured"))
            }
        };

        if plan.accept_immediately && plan.hold.is_some() {
            Self::stop_hold(session).await;
        }

        match dial_result {
            Ok(_) => {
                info!(session_id = %session.context.session_id, "Queue succeeded");
                Ok(())
            }
            Err(e) => {
                warn!(
                    session_id = %session.context.session_id,
                    error = %e,
                    "Queue targets exhausted"
                );
                Self::handle_queue_failure(session, next, inbox).await
            }
        }
    }

    async fn prepare_immediate_accept(session: &mut CallSession) -> Result<()> {
        info!(session_id = %session.context.session_id, "Queue accepting immediately");

        if session.caller_leg.answer_sdp.is_none() {
            let answer: String = session
                .build_caller_answer(
                    caller_negotiation::build_passthrough_caller_answer_codec_info(
                        session.caller_leg.offer_sdp.as_deref(),
                    ),
                )
                .await
                .map_err(|e| anyhow!("Failed to create SDP answer for queue: {}", e))?;
            session.set_answer(answer);
        }

        session.accept_call(None, None, None).await
    }

    async fn start_hold(session: &mut CallSession, hold_config: &QueueHoldConfig) {
        let Some(audio_file) = hold_config.audio_file.as_deref() else {
            return;
        };

        info!(
            session_id = %session.context.session_id,
            audio_file = %audio_file,
            loop_playback = hold_config.loop_playback,
            "Starting queue hold music"
        );

        if let Err(e) = session
            .caller_leg
            .media
            .create_or_switch_hold_track("queue-hold-music", audio_file, hold_config.loop_playback)
            .await
        {
            warn!("Failed to reuse queue hold track: {}", e);
            if let Err(err) = session
                .caller_leg
                .media
                .create_hold_track("queue-hold-music", audio_file, hold_config.loop_playback)
                .await
            {
                warn!(audio_file, error = %err, "create_queue_hold_track failed");
            }
        } else {
            debug!("Queue hold track ready");
        }

        let _ = session
            .bridge_runtime
            .suppress_or_pause_callee_forwarding(
                CallSession::CALLEE_TRACK_ID,
                session.caller_leg.peer.clone(),
            )
            .await;
    }

    async fn stop_hold(session: &mut CallSession) {
        info!(session_id = %session.context.session_id, "Stopping queue hold music");
        session.caller_leg.media.remove_track("queue-hold-music").await;
        let _ = session
            .bridge_runtime
            .resume_or_unpause_callee_forwarding(
                CallSession::CALLEE_TRACK_ID,
                session.caller_leg.peer.clone(),
            )
            .await;
    }

    pub async fn handle_queue_failure(
        session: &mut CallSession,
        next: Option<&DialplanFlow>,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(
            session_id = %session.context.session_id,
            "Handling queue failure with fallback"
        );

        if let Some(next_flow) = next {
            info!(
                session_id = %session.context.session_id,
                "Executing queue fallback flow"
            );
            return session.execute_flow(next_flow, inbox).await;
        }

        session.handle_failure(inbox).await
    }

    pub async fn run_target_attempt(
        session: &mut CallSession,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        if let Some(plan) = session.context.dialplan.flow.get_queue_plan_recursive() {
            if let Some(timeout) = plan.no_trying_timeout {
                debug!(
                    session_id = %session.context.session_id,
                    ?timeout,
                    "Applying no-trying timeout"
                );
                return match tokio::time::timeout(timeout, session.try_single_target(target)).await {
                    Ok(res) => res,
                    Err(_) => {
                        warn!(
                            session_id = %session.context.session_id,
                            "No-trying timeout triggered for target"
                        );
                        Err((
                            StatusCode::RequestTimeout,
                            Some("No-trying timeout".to_string()),
                        ))
                    }
                };
            }
        }

        session.try_single_target(target).await
    }

    pub fn should_retry_code(retry_codes: Option<&Vec<u16>>, code: StatusCode) -> bool {
        if let Some(retry_codes) = retry_codes {
            retry_codes.contains(&u16::from(code))
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QueueFlow;
    use rsip::StatusCode;

    #[test]
    fn test_should_retry_code_defaults_true_without_policy() {
        assert!(QueueFlow::should_retry_code(None, StatusCode::BusyHere));
    }

    #[test]
    fn test_should_retry_code_respects_retry_list() {
        let retry_codes = vec![408, 486];
        assert!(QueueFlow::should_retry_code(
            Some(&retry_codes),
            StatusCode::BusyHere
        ));
        assert!(!QueueFlow::should_retry_code(
            Some(&retry_codes),
            StatusCode::Decline
        ));
    }
}
