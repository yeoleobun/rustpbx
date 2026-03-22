use crate::call::app::{AppEventLoop, CallApp, CallController, ControllerEvent};
use crate::media::negotiate::MediaNegotiator;
use crate::proxy::proxy_call::caller_negotiation;
use crate::proxy::proxy_call::dtmf_listener::spawn_caller_dtmf_listener;
use crate::proxy::proxy_call::session::CallSession;
use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub(crate) struct AppRuntime;

impl AppRuntime {
    pub(crate) async fn build_call_app(
        session: &CallSession,
        app_name: &str,
        app_params: &Option<serde_json::Value>,
    ) -> Result<Box<dyn CallApp>> {
        #[allow(unused)]
        let registry = session
            .server
            .addon_registry
            .as_ref()
            .ok_or_else(|| anyhow!("No addon registry available"))?;

        match app_name {
            #[cfg(feature = "addon-voicemail")]
            "voicemail" => {
                let addon = registry
                    .get_addon("voicemail")
                    .ok_or_else(|| anyhow!("Voicemail addon not found"))?;
                let voicemail = addon
                    .as_any()
                    .downcast_ref::<crate::addons::voicemail::VoicemailAddon>()
                    .ok_or_else(|| anyhow!("Failed to downcast to VoicemailAddon"))?;

                let extension = app_params
                    .as_ref()
                    .and_then(|p| p.get("extension"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&session.context.original_callee);
                let caller_id = app_params
                    .as_ref()
                    .and_then(|p| p.get("caller_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&session.context.original_caller);

                let app = voicemail
                    .build_app(extension, caller_id)
                    .await
                    .map_err(|e| anyhow!("Failed to build VoicemailApp: {}", e))?;
                Ok(Box::new(app))
            }
            #[cfg(feature = "addon-voicemail")]
            "check_voicemail" => {
                let addon = registry
                    .get_addon("voicemail")
                    .ok_or_else(|| anyhow!("Voicemail addon not found"))?;
                let voicemail = addon
                    .as_any()
                    .downcast_ref::<crate::addons::voicemail::VoicemailAddon>()
                    .ok_or_else(|| anyhow!("Failed to downcast to VoicemailAddon"))?;

                let app = voicemail
                    .build_check_app()
                    .map_err(|e| anyhow!("Failed to build CheckVoicemailApp: {}", e))?;
                Ok(Box::new(app))
            }
            "ivr" => {
                let file = app_params
                    .as_ref()
                    .and_then(|p| p.get("file"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("IVR application requires 'file' parameter in app_params"))?;

                let app = crate::call::app::ivr::IvrApp::from_file(file)
                    .map_err(|e| anyhow!("Failed to build IvrApp: {}", e))?;
                Ok(Box::new(app))
            }
            "rwi" => {
                let gateway = session
                    .server
                    .rwi_gateway
                    .as_ref()
                    .ok_or_else(|| anyhow!("RWI gateway not configured on this server"))?
                    .clone();

                let context_name = app_params
                    .as_ref()
                    .and_then(|p| p.get("context"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string();

                let session_id = app_params
                    .as_ref()
                    .and_then(|p| p.get("session_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                Ok(Box::new(crate::rwi::app::RwiApp::new(
                    context_name,
                    session_id,
                    gateway,
                )))
            }
            _ => Err(anyhow!("Unknown application: {}", app_name)),
        }
    }

    pub async fn run(
        session: &mut CallSession,
        app_name: &str,
        app: Box<dyn CallApp>,
        auto_answer: bool,
    ) -> Result<()> {
        info!(
            session_id = %session.context.session_id,
            app_name,
            auto_answer,
            "Starting call application"
        );

        let (_app_shared, app_handle, mut app_cmd_rx, event_tx, event_rx) =
            Self::install_event_bridge(session);

        let (controller, timer_rx) = CallController::new(app_handle, event_rx);

        let app_context = Self::build_app_context(session);

        if let Err(e) = Self::prepare_auto_answer(session, auto_answer).await {
            Self::teardown(session);
            return Err(e);
        }

        let cancel_token = session.cancel_token.child_token();
        Self::spawn_dtmf_listener_if_needed(session, &event_tx, &cancel_token);

        let event_loop = AppEventLoop::new(app, controller, app_context, cancel_token.clone(), timer_rx);
        let mut event_loop_task = tokio::spawn(event_loop.run());
        let session_cancel = session.cancel_token.clone();

        let result = loop {
            tokio::select! {
                result = &mut event_loop_task => {
                    break match result {
                        Ok(Ok(())) => {
                            info!(
                                session_id = %session.context.session_id,
                                app_name,
                                "Application completed"
                            );
                            Ok(())
                        }
                        Ok(Err(e)) => {
                            warn!(
                                session_id = %session.context.session_id,
                                app_name,
                                error = %e,
                                "Application failed"
                            );
                            Err(e)
                        }
                        Err(join_err) => {
                            warn!(
                                session_id = %session.context.session_id,
                                app_name,
                                "Application task panicked: {}", join_err
                            );
                            Err(anyhow!("Application task panicked: {}", join_err))
                        }
                    };
                }
                cmd = app_cmd_rx.recv() => {
                    match cmd {
                        Some(action) => {
                            if let Err(e) = session.apply_session_action(action, None).await {
                                warn!(
                                    session_id = %session.context.session_id,
                                    error = %e,
                                    "Error handling app command"
                                );
                            }
                        }
                        None => {}
                    }
                }
                _ = session_cancel.cancelled() => {
                    event_loop_task.abort();
                    break Ok(());
                }
            }
        };

        Self::teardown(session);
        result
    }

    fn install_event_bridge(
        session: &mut CallSession,
    ) -> (
        CallSessionShared,
        CallSessionHandle,
        mpsc::UnboundedReceiver<crate::proxy::proxy_call::state::SessionAction>,
        mpsc::UnboundedSender<ControllerEvent>,
        mpsc::UnboundedReceiver<ControllerEvent>,
    ) {
        let app_shared = CallSessionShared::new(
            session.context.session_id.clone(),
            session.context.dialplan.direction,
            session.context.dialplan.caller.as_ref().map(|c| c.to_string()),
            session
                .context
                .dialplan
                .first_target()
                .map(|l| l.aor.to_string()),
            None,
        );
        let (app_handle, app_cmd_rx) = CallSessionHandle::with_shared(app_shared.clone());
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        session.app_event_tx = Some(event_tx.clone());
        session.shared.set_app_event_sender(Some(event_tx.clone()));
        (app_shared, app_handle, app_cmd_rx, event_tx, event_rx)
    }

    fn teardown(session: &mut CallSession) {
        session.app_event_tx = None;
        session.exported_leg.media.cancel_dtmf_listener();
        session.shared.set_dtmf_listener_cancel(None);
        session.shared.set_app_event_sender(None);
    }

    #[cfg(test)]
    fn teardown_for_test(shared: &CallSessionShared, media: &mut crate::proxy::proxy_call::media_endpoint::MediaEndpoint) {
        media.cancel_dtmf_listener();
        shared.set_dtmf_listener_cancel(None);
        shared.set_app_event_sender(None);
    }

    async fn prepare_auto_answer(session: &mut CallSession, auto_answer: bool) -> Result<()> {
        if !auto_answer {
            return Ok(());
        }

        if session.exported_leg.media.answer_sdp.is_none() {
            match session
                .build_caller_answer(
                    caller_negotiation::build_passthrough_caller_answer_codec_info(
                        session.exported_leg.media.offer_sdp.as_deref(),
                    ),
                )
                .await
            {
                Ok(answer) => session.set_answer(answer),
                Err(e) => {
                    warn!(
                        session_id = %session.context.session_id,
                        error = %e,
                        "Failed to create SDP answer for application"
                    );
                    return Err(anyhow!("Failed to create SDP answer: {}", e));
                }
            }
        }

        if let Err(e) = session.accept_call(None, None, None).await {
            warn!(
                session_id = %session.context.session_id,
                error = %e,
                "Failed to accept call for application"
            );
            return Err(anyhow!("Failed to accept call: {}", e));
        }

        Ok(())
    }

    fn build_app_context(session: &CallSession) -> crate::call::app::ApplicationContext {
        let db = session.server.database.clone().unwrap_or_else(|| {
            warn!(
                session_id = %session.context.session_id,
                "No database connection for application context, using empty stub"
            );
            sea_orm::DatabaseConnection::Disconnected
        });

        let storage = session.server.storage.clone().unwrap_or_else(|| {
            warn!(
                session_id = %session.context.session_id,
                "No storage for application context, using temp dir"
            );
            let cfg = crate::storage::StorageConfig::Local {
                path: std::env::temp_dir()
                    .join("rustpbx-fallback")
                    .to_string_lossy()
                    .to_string(),
            };
            crate::storage::Storage::new(&cfg).expect("fallback storage")
        });

        let call_info = crate::call::app::CallInfo {
            session_id: session.context.session_id.clone(),
            caller: session.context.original_caller.clone(),
            callee: session.context.original_callee.clone(),
            direction: session.context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
        };

        crate::call::app::ApplicationContext::new(
            db,
            call_info,
            Arc::new(crate::config::Config::default()),
            storage,
        )
    }

    fn spawn_dtmf_listener_if_needed(
        session: &mut CallSession,
        event_tx: &mpsc::UnboundedSender<ControllerEvent>,
        cancel_token: &CancellationToken,
    ) {
        let caller_dtmf_codecs = session
            .exported_leg
            .media.offer_sdp
            .as_deref()
            .map(MediaNegotiator::extract_dtmf_codecs)
            .unwrap_or_default();
        if caller_dtmf_codecs.is_empty() {
            return;
        }

        let exported_peer = session.exported_leg.media.peer.clone();
        let dtmf_event_tx = event_tx.clone();
        let dtmf_cancel = cancel_token.child_token();
        session.exported_leg.media.set_dtmf_listener_cancel(dtmf_cancel.clone());
        session
            .shared
            .set_dtmf_listener_cancel(Some(dtmf_cancel.clone()));
        crate::utils::spawn(async move {
            spawn_caller_dtmf_listener(
                exported_peer,
                dtmf_event_tx,
                caller_dtmf_codecs,
                dtmf_cancel,
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::AppRuntime;
    use crate::call::DialDirection;
    use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
    use crate::proxy::proxy_call::state::CallSessionShared;
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_app_event_bridge_sender_is_installed_and_cleared() {
        let shared = CallSessionShared::new(
            "app-runtime-test".to_string(),
            DialDirection::Inbound,
            Some("sip:1001@127.0.0.1".to_string()),
            Some("sip:4000@127.0.0.1".to_string()),
            None,
        );
        let (tx, _rx) = mpsc::unbounded_channel();

        shared.set_app_event_sender(Some(tx));
        assert!(shared.send_app_event(crate::call::app::ControllerEvent::Custom(
            "probe".to_string(),
            serde_json::json!({"ok": true}),
        )));

        shared.set_app_event_sender(None);
        assert!(!shared.send_app_event(crate::call::app::ControllerEvent::Custom(
            "probe".to_string(),
            serde_json::json!({"ok": true}),
        )));
    }

    #[tokio::test]
    async fn test_dtmf_listener_cancel_is_installed_and_cleared() {
        let shared = CallSessionShared::new(
            "dtmf-runtime-test".to_string(),
            DialDirection::Inbound,
            Some("sip:1001@127.0.0.1".to_string()),
            Some("sip:4000@127.0.0.1".to_string()),
            None,
        );
        let peer = Arc::new(MockMediaPeer::new());
        let mut media = MediaEndpoint::new(peer, None);
        let token = CancellationToken::new();

        media.set_dtmf_listener_cancel(token.clone());
        shared.set_dtmf_listener_cancel(Some(token.clone()));
        shared.cancel_dtmf_listener();
        assert!(token.is_cancelled());

        let token2 = CancellationToken::new();
        media.set_dtmf_listener_cancel(token2.clone());
        AppRuntime::teardown_for_test(&shared, &mut media);
        assert!(token2.is_cancelled());
        assert!(!shared.send_app_event(crate::call::app::ControllerEvent::Custom(
            "probe".to_string(),
            serde_json::json!({"ok": true}),
        )));
    }
}
