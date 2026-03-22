use crate::call::{DialStrategy, DialplanFlow};
use crate::proxy::proxy_call::proxy_runtime::ProxySessionRuntime;
use crate::proxy::proxy_call::session::{ActionInbox, CallSession, FlowFailureHandling};
use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};
use rsip::StatusCode;
use tokio::time::timeout;
use tracing::debug;

pub(crate) struct DialplanRuntime;

impl DialplanRuntime {
    pub(crate) async fn execute_dialplan(
        session: &mut CallSession,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        session.shared.transition_to_routing();
        session.process_pending_actions(inbox.as_deref_mut()).await?;
        if session.context.dialplan.is_empty() {
            session.set_error(
                StatusCode::ServerInternalError,
                Some("No targets in dialplan".to_string()),
                None,
            );
            return Err(anyhow!("Dialplan has no targets"));
        }
        if Self::try_forwarding_before_dial(session, inbox.as_deref_mut()).await? {
            return Ok(());
        }
        Self::execute_flow(
            session,
            &session.context.dialplan.flow.clone(),
            inbox,
            FlowFailureHandling::Handle,
        )
        .await
    }

    pub(crate) async fn try_forwarding_before_dial(
        session: &mut CallSession,
        inbox: ActionInbox<'_>,
    ) -> Result<bool> {
        let Some(config) = session.immediate_forwarding_config().cloned() else {
            return Ok(false);
        };
        tracing::info!(
            session_id = %session.context.session_id,
            endpoint = ?config.endpoint,
            "Call forwarding (always) engaged"
        );
        ProxySessionRuntime::transfer_to_endpoint(session, &config.endpoint, inbox).await?;
        Ok(true)
    }

    pub(crate) fn execute_flow<'a>(
        session: &'a mut CallSession,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
        handling: FlowFailureHandling,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match flow {
                DialplanFlow::Targets(strategy) => match handling {
                    FlowFailureHandling::Handle => Self::execute_targets(session, strategy, inbox).await,
                    FlowFailureHandling::Propagate => ProxySessionRuntime::run_targets(session, strategy, inbox).await,
                },
                DialplanFlow::Queue { plan, next } => {
                    crate::proxy::proxy_call::queue_flow::QueueFlow::execute_queue_plan(
                        session,
                        plan,
                        Some(&**next),
                        inbox,
                    )
                    .await
                }
                DialplanFlow::Application {
                    app_name,
                    app_params,
                    auto_answer,
                } => session.run_application(app_name, app_params.clone(), *auto_answer).await,
            }
        }
        .boxed()
    }

    async fn execute_targets(
        session: &mut CallSession,
        strategy: &DialStrategy,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        session.process_pending_actions(inbox.as_deref_mut()).await?;
        let timeout_duration = session.forwarding_timeout();

        if let Some(duration) = timeout_duration {
            match timeout(duration, ProxySessionRuntime::run_targets(session, strategy, inbox.as_deref_mut())).await {
                Ok(outcome) => match outcome {
                    Ok(_) => {
                        debug!(session_id = %session.context.session_id, "Dialplan executed successfully");
                        Ok(())
                    }
                    Err(_) => ProxySessionRuntime::handle_failure(session, inbox).await,
                },
                Err(_) => {
                    session.set_error(
                        StatusCode::RequestTimeout,
                        Some("Call forwarding timeout elapsed".to_string()),
                        None,
                    );
                    Err(anyhow!("forwarding timeout elapsed"))
                }
            }
        } else {
            match ProxySessionRuntime::run_targets(session, strategy, inbox.as_deref_mut()).await {
                Ok(_) => {
                    debug!(session_id = %session.context.session_id, "Dialplan executed successfully");
                    Ok(())
                }
                Err(_) => ProxySessionRuntime::handle_failure(session, inbox).await,
            }
        }
    }
}
