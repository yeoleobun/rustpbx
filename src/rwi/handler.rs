use crate::handler::middleware::clientaddr::ClientAddr;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerRef;
use crate::rwi::Result;
use crate::rwi::auth::{RwiAuth, RwiIdentity};
use crate::rwi::gateway::RwiGateway;
use crate::rwi::processor::RwiCommandProcessor;
use crate::rwi::proto::{
    CallIdData, ConferenceIdData, ConferenceMemberData, ResponseStatus, RwiErrorCode, RwiResponse,
    RwiResponseData, TrackIdData, TransferAttendedData,
};
use crate::rwi::session::{OwnershipMode, RwiCommandPayload};
use crate::rwi::types::{HandleTextMessageError, RwiRequestEnvelope, RwiWireResponse};
use axum::{
    Extension,
    extract::Query,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::HeaderMap,
    response::Response,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

pub async fn rwi_ws_handler(
    _client_addr: ClientAddr,
    ws: WebSocketUpgrade,
    Query(params): Query<std::collections::HashMap<String, String>>,
    Extension(auth): Extension<Arc<RwiAuth>>,
    Extension(gateway): Extension<Arc<RwLock<RwiGateway>>>,
    Extension(call_registry): Extension<Arc<ActiveProxyCallRegistry>>,
    Extension(sip_server): Extension<Option<SipServerRef>>,
    headers: HeaderMap,
) -> Result<Response> {
    let identity = authenticate_request(&headers, &params, auth).await?;

    Ok(ws.protocols(["rwi-v1"]).on_upgrade(async move |socket| {
        handle_websocket(socket, identity, gateway, call_registry, sip_server).await;
    }))
}

fn extract_token(
    headers: &HeaderMap,
    query_params: &std::collections::HashMap<String, String>,
) -> Result<String> {
    if let Some(auth_header) = headers.get("authorization") {
        let auth_str = auth_header
            .to_str()
            .map_err(|_| crate::rwi::AuthError::InvalidAuthorizationHeader)?;
        if let Some(token) = auth_str.strip_prefix("Bearer ") {
            return Ok(token.to_string());
        }
    }

    query_params
        .get("token")
        .cloned()
        .ok_or(crate::rwi::AuthError::MissingToken.into())
}

async fn authenticate_request(
    headers: &HeaderMap,
    query_params: &std::collections::HashMap<String, String>,
    auth: Arc<RwiAuth>,
) -> Result<RwiIdentity> {
    let token = extract_token(headers, query_params)?;
    auth.validate_token(&token)
}

/// Single unified WebSocket session loop.
///
/// One loop owns the websocket sender/receiver and multiplexes:
/// - client text frames -> command processing -> response write
/// - gateway event channel -> event serialization -> event write
async fn handle_websocket(
    socket: WebSocket,
    identity: RwiIdentity,
    gateway: Arc<RwLock<RwiGateway>>,
    call_registry: Arc<ActiveProxyCallRegistry>,
    sip_server: Option<SipServerRef>,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    let p = RwiCommandProcessor::new(call_registry, gateway.clone());
    let p = if let Some(server) = sip_server {
        p.with_sip_server(server)
    } else {
        p
    };
    let processor = Arc::new(p);

    let mut gw = gateway.write().await;
    let session = gw.create_session(identity.clone());
    let session_id = session.read().await.id.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    gw.set_session_event_sender(&session_id, event_tx);

    loop {
        tokio::select! {
            ws_msg = ws_receiver.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        let response = RwiWireResponse(handle_text_message(
                            text.as_ref(),
                            processor.clone(),
                            &session_id,
                            gateway.clone(),
                        )
                        .await);
                        if let Ok(json) = serde_json::to_string(&response) {
                            if ws_sender.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some(value) => {
                        if let Ok(json) = serde_json::to_string(&value) {
                            if ws_sender.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    let mut gw = gateway.write().await;
    gw.remove_session(&session_id).await;
}

/// Process one text frame from the WebSocket.
///
/// Returns the typed response or a typed protocol error.
async fn handle_text_message(
    text: &str,
    processor: Arc<RwiCommandProcessor>,
    session_id: &str,
    gateway: Arc<RwLock<RwiGateway>>,
) -> Result<RwiResponse> {
    let request: RwiRequestEnvelope = serde_json::from_str(text)?;

    let action_id = request
        .action_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let action = request.action.ok_or_else(|| {
        HandleTextMessageError::new(
            action_id.clone(),
            RwiErrorCode::MissingAction,
            "action field required",
        )
    })?;

    let command = parse_action(&action_id, &action, request.params.as_ref())?;

    match &command {
        RwiCommandPayload::Subscribe { contexts } => {
            let mut gw = gateway.write().await;
            gw.subscribe(&session_id.to_string(), contexts.clone())
                .await;
        }
        RwiCommandPayload::Unsubscribe { contexts } => {
            let mut gw = gateway.write().await;
            gw.unsubscribe(&session_id.to_string(), contexts).await;
        }
        RwiCommandPayload::DetachCall { call_id } => {
            let mut gw = gateway.write().await;
            if gw
                .release_call_ownership(&session_id.to_string(), call_id)
                .await
            {
                gw.detach_supervisor(&session_id.to_string(), call_id).await;
            }
        }
        _ => {}
    }

    let cmd_result = processor.process_command(&action_id, command).await?;

    let data = match cmd_result {
        crate::rwi::processor::CommandResult::ListCalls(calls) => {
            Some(RwiResponseData::CallList(calls))
        }
        crate::rwi::processor::CommandResult::CallFound { call_id } => {
            Some(RwiResponseData::CallId(CallIdData { call_id }))
        }
        crate::rwi::processor::CommandResult::Originated { call_id } => {
            {
                let mut gw = gateway.write().await;
                let _ = gw
                    .claim_call_ownership(
                        &session_id.to_string(),
                        call_id.clone(),
                        OwnershipMode::Control,
                    )
                    .await;
            }
            Some(RwiResponseData::CallId(CallIdData { call_id }))
        }
        crate::rwi::processor::CommandResult::MediaPlay { track_id } => {
            Some(RwiResponseData::TrackId(TrackIdData { track_id }))
        }
        crate::rwi::processor::CommandResult::TransferAttended {
            original_call_id,
            consultation_call_id,
        } => Some(RwiResponseData::TransferAttended(TransferAttendedData {
            original_call_id,
            consultation_call_id,
        })),
        crate::rwi::processor::CommandResult::ConferenceCreated { conf_id } => {
            Some(RwiResponseData::ConferenceId(ConferenceIdData { conf_id }))
        }
        crate::rwi::processor::CommandResult::ConferenceMemberAdded { conf_id, call_id } => {
            Some(RwiResponseData::ConferenceMember(ConferenceMemberData {
                conf_id,
                call_id,
            }))
        }
        crate::rwi::processor::CommandResult::ConferenceMemberRemoved { conf_id, call_id } => {
            Some(RwiResponseData::ConferenceMember(ConferenceMemberData {
                conf_id,
                call_id,
            }))
        }
        crate::rwi::processor::CommandResult::ConferenceMemberMuted { conf_id, call_id } => {
            Some(RwiResponseData::ConferenceMember(ConferenceMemberData {
                conf_id,
                call_id,
            }))
        }
        crate::rwi::processor::CommandResult::ConferenceMemberUnmuted { conf_id, call_id } => {
            Some(RwiResponseData::ConferenceMember(ConferenceMemberData {
                conf_id,
                call_id,
            }))
        }
        crate::rwi::processor::CommandResult::ConferenceDestroyed { conf_id } => {
            Some(RwiResponseData::ConferenceId(ConferenceIdData { conf_id }))
        }
        crate::rwi::processor::CommandResult::Success => None,
    };

    Ok(RwiResponse {
        action_id,
        response: ResponseStatus::Success,
        data,
        error: None,
    })
}

fn parse_action(
    action_id: &str,
    action: &str,
    params: Option<&serde_json::Value>,
) -> Result<RwiCommandPayload> {
    let mut request_map = serde_json::Map::new();
    request_map.insert(
        "action".to_string(),
        serde_json::Value::String(action.to_string()),
    );

    if let Some(params) = params {
        let include_params = match params {
            serde_json::Value::Null => false,
            serde_json::Value::Object(obj) => !obj.is_empty(),
            _ => true,
        };
        if include_params {
            request_map.insert("params".to_string(), params.clone());
        }
    }

    let req: crate::rwi::session::RwiRequest =
        serde_json::from_value(serde_json::Value::Object(request_map)).map_err(|e| {
            HandleTextMessageError::new(action_id, RwiErrorCode::UnknownAction, e.to_string())
        })?;

    Ok(req.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
    use crate::rwi::AuthError;
    use crate::rwi::processor::RwiCommandProcessor;
    use std::sync::Arc;

    fn create_test_processor() -> Arc<RwiCommandProcessor> {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        Arc::new(RwiCommandProcessor::new(registry, gateway))
    }

    async fn process_msg(json: &str) -> serde_json::Value {
        let processor = create_test_processor();
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let resp =
            RwiWireResponse(handle_text_message(json, processor, "test-session", gateway).await);
        serde_json::to_value(resp).expect("response should be valid JSON")
    }

    #[test]
    fn test_extract_token_from_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer test-token-123".parse().unwrap());
        let params = std::collections::HashMap::new();
        let token = extract_token(&headers, &params);
        assert!(matches!(token, Ok(ref value) if value == "test-token-123"));
    }

    #[test]
    fn test_extract_token_from_query() {
        let headers = axum::http::HeaderMap::new();
        let mut params = std::collections::HashMap::new();
        params.insert("token".to_string(), "query-token-456".to_string());
        let token = extract_token(&headers, &params);
        assert!(matches!(token, Ok(ref value) if value == "query-token-456"));
    }

    #[test]
    fn test_extract_token_header_priority() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer header-token".parse().unwrap());
        let mut params = std::collections::HashMap::new();
        params.insert("token".to_string(), "query-token".to_string());
        let token = extract_token(&headers, &params);
        assert!(matches!(token, Ok(ref value) if value == "header-token"));
    }

    #[test]
    fn test_extract_token_missing() {
        let headers = axum::http::HeaderMap::new();
        let params = std::collections::HashMap::new();
        assert!(matches!(
            extract_token(&headers, &params),
            Err(crate::rwi::Error::Auth(AuthError::MissingToken))
        ));
    }

    // ---- Command processing tests ----

    #[tokio::test]
    async fn test_session_subscribe_returns_success() {
        let v = process_msg(
            r#"{"action": "session.subscribe", "params": {"contexts": ["ctx1", "ctx2"]}}"#,
        )
        .await;
        assert_eq!(v["response"], "success");
    }

    #[tokio::test]
    async fn test_session_list_calls_returns_success_with_action_id() {
        let v = process_msg(r#"{"action": "session.list_calls", "action_id": "req-1"}"#).await;
        assert_eq!(v["response"], "success");
        assert_eq!(v["action_id"], "req-1");
    }

    #[tokio::test]
    async fn test_call_answer_not_found_returns_error() {
        let v = process_msg(r#"{"action": "call.answer", "params": {"call_id": "missing"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn test_call_hangup_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "call.hangup", "params": {"call_id": "missing", "reason": "normal"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
    }

    #[tokio::test]
    async fn test_call_bridge_not_found_returns_error() {
        let v = process_msg(r#"{"action": "call.bridge", "params": {"leg_a": "a", "leg_b": "b"}}"#)
            .await;
        assert_eq!(v["response"], "error");
    }

    #[tokio::test]
    async fn test_media_play_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "media.play", "params": {"call_id": "c", "source": {"type": "file", "uri": "x.wav"}}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn test_unknown_action_returns_error() {
        let v = process_msg(r#"{"action": "bad.action", "params": {}}"#).await;
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "unknown_action");
    }

    #[tokio::test]
    async fn test_invalid_json_returns_error() {
        let processor = create_test_processor();
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let resp =
            RwiWireResponse(handle_text_message("not json", processor, "sess", gateway).await);
        let v: serde_json::Value = serde_json::to_value(resp).unwrap();
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "parse_error");
    }

    #[tokio::test]
    async fn test_missing_action_returns_error() {
        let v = process_msg(r#"{"params": {}}"#).await;
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "missing_action");
    }

    #[tokio::test]
    async fn test_response_preserves_action_id() {
        let v =
            process_msg(r#"{"action": "session.list_calls", "action_id": "my-custom-id"}"#).await;
        assert_eq!(v["action_id"], "my-custom-id");
    }

    #[tokio::test]
    async fn test_call_ring_not_found() {
        let v = process_msg(r#"{"action": "call.ring", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_eq!(v["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn test_call_reject_not_found() {
        let v = process_msg(
            r#"{"action": "call.reject", "params": {"call_id": "nope", "reason": "busy"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
    }

    #[tokio::test]
    async fn test_call_transfer_not_found() {
        let v = process_msg(
            r#"{"action": "call.transfer", "params": {"call_id": "nope", "target": "sip:x@y"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
    }

    #[tokio::test]
    async fn test_call_unbridge_not_found() {
        let v = process_msg(r#"{"action": "call.unbridge", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
    }

    // ---- media.stream_start ----

    #[test]
    fn test_media_stream_start_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "c1",
            "direction": "sendonly",
            "format": { "codec": "PCMA", "sample_rate": 16000, "channels": 2, "ptime_ms": 20 }
        });
        let cmd = parse_action("", "media.stream_start", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::MediaStreamStart(r) => {
                assert_eq!(r.call_id, "c1");
                assert_eq!(r.direction, "sendonly");
                assert_eq!(r.format.codec, "PCMA");
                assert_eq!(r.format.sample_rate, 16000);
                assert_eq!(r.format.channels, 2);
                assert_eq!(r.format.ptime_ms, Some(20));
            }
            _ => panic!("expected MediaStreamStart"),
        }
    }

    #[test]
    fn test_media_stream_start_defaults() {
        let params = serde_json::json!({ "call_id": "c1" });
        let cmd = parse_action("", "media.stream_start", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::MediaStreamStart(r) => {
                assert_eq!(r.direction, "sendrecv");
                assert_eq!(r.format.codec, "PCMU");
                assert_eq!(r.format.sample_rate, 8000);
                assert_eq!(r.format.channels, 1);
                assert_eq!(r.format.ptime_ms, None);
            }
            _ => panic!("expected MediaStreamStart"),
        }
    }

    #[tokio::test]
    async fn test_media_stream_start_not_found_returns_error() {
        let v =
            process_msg(r#"{"action": "media.stream_start", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- media.stream_stop ----

    #[test]
    fn test_media_stream_stop_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "c2" });
        let cmd = parse_action("", "media.stream_stop", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::MediaStreamStop { call_id } if call_id == "c2"));
    }

    #[tokio::test]
    async fn test_media_stream_stop_not_found_returns_error() {
        let v =
            process_msg(r#"{"action": "media.stream_stop", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- media.inject_start ----

    #[test]
    fn test_media_inject_start_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "c3",
            "format": { "codec": "PCMU", "sample_rate": 8000, "channels": 1 }
        });
        let cmd = parse_action("", "media.inject_start", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::MediaInjectStart(r) => {
                assert_eq!(r.call_id, "c3");
                assert_eq!(r.format.codec, "PCMU");
                assert_eq!(r.format.sample_rate, 8000);
            }
            _ => panic!("expected MediaInjectStart"),
        }
    }

    #[tokio::test]
    async fn test_media_inject_start_not_found_returns_error() {
        let v =
            process_msg(r#"{"action": "media.inject_start", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- media.inject_stop ----

    #[test]
    fn test_media_inject_stop_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "c4" });
        let cmd = parse_action("", "media.inject_stop", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::MediaInjectStop { call_id } if call_id == "c4"));
    }

    #[tokio::test]
    async fn test_media_inject_stop_not_found_returns_error() {
        let v =
            process_msg(r#"{"action": "media.inject_stop", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- record.start ----

    #[test]
    fn test_record_start_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "r1",
            "mode": "split",
            "beep": true,
            "max_duration_secs": 3600,
            "storage": { "backend": "s3", "path": "bucket/key" }
        });
        let cmd = parse_action("", "record.start", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::RecordStart(r) => {
                assert_eq!(r.call_id, "r1");
                assert_eq!(r.mode, "split");
                assert_eq!(r.beep, Some(true));
                assert_eq!(r.max_duration_secs, Some(3600));
                assert_eq!(r.storage.backend, "s3");
                assert_eq!(r.storage.path, "bucket/key");
            }
            _ => panic!("expected RecordStart"),
        }
    }

    #[test]
    fn test_record_start_storage_defaults() {
        let params = serde_json::json!({ "call_id": "r1" });
        let cmd = parse_action("", "record.start", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::RecordStart(r) => {
                assert_eq!(r.mode, "mixed");
                assert_eq!(r.beep, None);
                assert_eq!(r.max_duration_secs, None);
                assert_eq!(r.storage.backend, "file");
                assert_eq!(r.storage.path, "");
            }
            _ => panic!("expected RecordStart"),
        }
    }

    #[tokio::test]
    async fn test_record_start_not_found_returns_error() {
        let v = process_msg(r#"{"action": "record.start", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- record.pause ----

    #[test]
    fn test_record_pause_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "r2" });
        let cmd = parse_action("", "record.pause", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::RecordPause { call_id } if call_id == "r2"));
    }

    #[tokio::test]
    async fn test_record_pause_not_found_returns_error() {
        let v = process_msg(r#"{"action": "record.pause", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- record.resume ----

    #[test]
    fn test_record_resume_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "r3" });
        let cmd = parse_action("", "record.resume", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::RecordResume { call_id } if call_id == "r3"));
    }

    #[tokio::test]
    async fn test_record_resume_not_found_returns_error() {
        let v = process_msg(r#"{"action": "record.resume", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- record.stop ----

    #[test]
    fn test_record_stop_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "r4" });
        let cmd = parse_action("", "record.stop", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::RecordStop { call_id } if call_id == "r4"));
    }

    #[tokio::test]
    async fn test_record_stop_not_found_returns_error() {
        let v = process_msg(r#"{"action": "record.stop", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- queue.enqueue ----

    #[test]
    fn test_queue_enqueue_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "q1",
            "queue_id": "support",
            "priority": 5,
            "skills": ["en", "tech"],
            "max_wait_secs": 120
        });
        let cmd = parse_action("", "queue.enqueue", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::QueueEnqueue(r) => {
                assert_eq!(r.call_id, "q1");
                assert_eq!(r.queue_id, "support");
                assert_eq!(r.priority, Some(5));
                assert_eq!(r.skills, Some(vec!["en".to_string(), "tech".to_string()]));
                assert_eq!(r.max_wait_secs, Some(120));
            }
            _ => panic!("expected QueueEnqueue"),
        }
    }

    #[test]
    fn test_queue_enqueue_skills_array_parsing() {
        let params = serde_json::json!({
            "call_id": "q2",
            "queue_id": "billing",
            "skills": ["billing", "french", "vip"]
        });
        let cmd = parse_action("", "queue.enqueue", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::QueueEnqueue(r) => {
                let skills = r.skills.expect("skills should be Some");
                assert_eq!(skills.len(), 3);
                assert!(skills.contains(&"french".to_string()));
            }
            _ => panic!("expected QueueEnqueue"),
        }
    }

    #[tokio::test]
    async fn test_queue_enqueue_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "queue.enqueue", "params": {"call_id": "nope", "queue_id": "q1"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- queue.dequeue ----

    #[test]
    fn test_queue_dequeue_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "q3" });
        let cmd = parse_action("", "queue.dequeue", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::QueueDequeue { call_id } if call_id == "q3"));
    }

    #[tokio::test]
    async fn test_queue_dequeue_not_found_returns_error() {
        let v = process_msg(r#"{"action": "queue.dequeue", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- queue.hold ----

    #[test]
    fn test_queue_hold_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "q4" });
        let cmd = parse_action("", "queue.hold", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::QueueHold { call_id } if call_id == "q4"));
    }

    #[tokio::test]
    async fn test_queue_hold_not_found_returns_error() {
        let v = process_msg(r#"{"action": "queue.hold", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- queue.unhold ----

    #[test]
    fn test_queue_unhold_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "q5" });
        let cmd = parse_action("", "queue.unhold", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::QueueUnhold { call_id } if call_id == "q5"));
    }

    #[tokio::test]
    async fn test_queue_unhold_not_found_returns_error() {
        let v = process_msg(r#"{"action": "queue.unhold", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- supervisor.listen ----

    #[test]
    fn test_supervisor_listen_parses_to_correct_variant() {
        let params = serde_json::json!({
            "supervisor_call_id": "sup1",
            "target_call_id": "tgt1"
        });
        let cmd = parse_action("", "supervisor.listen", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => {
                assert_eq!(supervisor_call_id, "sup1");
                assert_eq!(target_call_id, "tgt1");
            }
            _ => panic!("expected SupervisorListen"),
        }
    }

    #[tokio::test]
    async fn test_supervisor_listen_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "supervisor.listen", "params": {"supervisor_call_id": "s", "target_call_id": "t"}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- supervisor.whisper ----

    #[test]
    fn test_supervisor_whisper_parses_to_correct_variant() {
        let params = serde_json::json!({
            "supervisor_call_id": "sup2",
            "target_call_id": "tgt2",
            "agent_leg": "leg-a"
        });
        let cmd = parse_action("", "supervisor.whisper", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                assert_eq!(supervisor_call_id, "sup2");
                assert_eq!(target_call_id, "tgt2");
                assert_eq!(agent_leg, "leg-a");
            }
            _ => panic!("expected SupervisorWhisper"),
        }
    }

    #[tokio::test]
    async fn test_supervisor_whisper_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "supervisor.whisper", "params": {"supervisor_call_id": "s", "target_call_id": "t"}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- supervisor.barge ----

    #[test]
    fn test_supervisor_barge_parses_to_correct_variant() {
        let params = serde_json::json!({
            "supervisor_call_id": "sup3",
            "target_call_id": "tgt3",
            "agent_leg": "leg-b"
        });
        let cmd = parse_action("", "supervisor.barge", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                assert_eq!(supervisor_call_id, "sup3");
                assert_eq!(target_call_id, "tgt3");
                assert_eq!(agent_leg, "leg-b");
            }
            _ => panic!("expected SupervisorBarge"),
        }
    }

    #[tokio::test]
    async fn test_supervisor_barge_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "supervisor.barge", "params": {"supervisor_call_id": "s", "target_call_id": "t"}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- supervisor.stop ----

    #[test]
    fn test_supervisor_stop_parses_to_correct_variant() {
        let params = serde_json::json!({
            "supervisor_call_id": "sup4",
            "target_call_id": "tgt4"
        });
        let cmd = parse_action("", "supervisor.stop", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => {
                assert_eq!(supervisor_call_id, "sup4");
                assert_eq!(target_call_id, "tgt4");
            }
            _ => panic!("expected SupervisorStop"),
        }
    }

    #[tokio::test]
    async fn test_supervisor_stop_not_found_returns_error() {
        // supervisor.stop does not require an existing call (it cleans up state idempotently);
        // the important thing is it is no longer an unknown_action.
        let v = process_msg(
            r#"{"action": "supervisor.stop", "params": {"supervisor_call_id": "s", "target_call_id": "t"}}"#,
        ).await;
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- sip.message ----

    #[test]
    fn test_sip_message_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "sip1",
            "content_type": "text/html",
            "body": "<b>hello</b>"
        });
        let cmd = parse_action("", "sip.message", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SipMessage {
                call_id,
                content_type,
                body,
            } => {
                assert_eq!(call_id, "sip1");
                assert_eq!(content_type, "text/html");
                assert_eq!(body, "<b>hello</b>");
            }
            _ => panic!("expected SipMessage"),
        }
    }

    #[test]
    fn test_sip_message_defaults() {
        let params = serde_json::json!({ "call_id": "sip1" });
        let cmd = parse_action("", "sip.message", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SipMessage {
                content_type, body, ..
            } => {
                assert_eq!(content_type, "text/plain");
                assert_eq!(body, "");
            }
            _ => panic!("expected SipMessage"),
        }
    }

    #[tokio::test]
    async fn test_sip_message_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "sip.message", "params": {"call_id": "nope", "body": "hi"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- sip.notify ----

    #[test]
    fn test_sip_notify_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "sip2",
            "event": "presence",
            "content_type": "application/pidf+xml",
            "body": "<pidf/>"
        });
        let cmd = parse_action("", "sip.notify", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SipNotify {
                call_id,
                event,
                content_type,
                body,
            } => {
                assert_eq!(call_id, "sip2");
                assert_eq!(event, "presence");
                assert_eq!(content_type, "application/pidf+xml");
                assert_eq!(body, "<pidf/>");
            }
            _ => panic!("expected SipNotify"),
        }
    }

    #[test]
    fn test_sip_notify_defaults() {
        let params = serde_json::json!({ "call_id": "sip2" });
        let cmd = parse_action("", "sip.notify", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::SipNotify {
                event,
                content_type,
                body,
                ..
            } => {
                assert_eq!(event, "");
                assert_eq!(content_type, "application/json");
                assert_eq!(body, "");
            }
            _ => panic!("expected SipNotify"),
        }
    }

    #[tokio::test]
    async fn test_sip_notify_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "sip.notify", "params": {"call_id": "nope", "event": "check-sync"}}"#,
        )
        .await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- sip.options_ping ----

    #[test]
    fn test_sip_options_ping_parses_to_correct_variant() {
        let params = serde_json::json!({ "call_id": "sip3" });
        let cmd = parse_action("", "sip.options_ping", Some(&params)).unwrap();
        assert!(matches!(cmd, RwiCommandPayload::SipOptionsPing { call_id } if call_id == "sip3"));
    }

    #[tokio::test]
    async fn test_sip_options_ping_not_found_returns_error() {
        let v =
            process_msg(r#"{"action": "sip.options_ping", "params": {"call_id": "nope"}}"#).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- action_id preservation for new commands ----

    #[tokio::test]
    async fn test_new_commands_preserve_action_id() {
        let v = process_msg(
            r#"{"action": "record.stop", "action_id": "my-id-42", "params": {"call_id": "nope"}}"#,
        )
        .await;
        assert_eq!(v["action_id"], "my-id-42");
    }

    // ---- call.transfer.attended ----

    #[test]
    fn test_transfer_attended_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "orig-1",
            "target": "sip:agent@local",
            "timeout_secs": 30
        });
        let cmd = parse_action("", "call.transfer.attended", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::TransferAttended {
                call_id,
                target,
                timeout_secs,
            } => {
                assert_eq!(call_id, "orig-1");
                assert_eq!(target, "sip:agent@local");
                assert_eq!(timeout_secs, Some(30));
            }
            _ => panic!("expected TransferAttended"),
        }
    }

    #[test]
    fn test_transfer_attended_optional_timeout() {
        let params = serde_json::json!({ "call_id": "c1", "target": "sip:x@y" });
        let cmd = parse_action("", "call.transfer.attended", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::TransferAttended { timeout_secs, .. } => {
                assert_eq!(timeout_secs, None);
            }
            _ => panic!("expected TransferAttended"),
        }
    }

    #[tokio::test]
    async fn test_transfer_attended_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "call.transfer.attended", "params": {"call_id": "nope", "target": "sip:x@y"}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- call.transfer.complete ----

    #[test]
    fn test_transfer_complete_parses_to_correct_variant() {
        let params = serde_json::json!({
            "call_id": "orig-2",
            "consultation_call_id": "consult-42"
        });
        let cmd = parse_action("", "call.transfer.complete", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::TransferComplete {
                call_id,
                consultation_call_id,
            } => {
                assert_eq!(call_id, "orig-2");
                assert_eq!(consultation_call_id, "consult-42");
            }
            _ => panic!("expected TransferComplete"),
        }
    }

    #[tokio::test]
    async fn test_transfer_complete_not_found_returns_error() {
        let v = process_msg(
            r#"{"action": "call.transfer.complete", "params": {"call_id": "nope", "consultation_call_id": "c2"}}"#,
        ).await;
        assert_eq!(v["response"], "error");
        assert_ne!(v["error"]["code"], "unknown_action");
    }

    // ---- call.transfer.cancel ----

    #[test]
    fn test_transfer_cancel_parses_to_correct_variant() {
        let params = serde_json::json!({ "consultation_call_id": "consult-99" });
        let cmd = parse_action("", "call.transfer.cancel", Some(&params)).unwrap();
        match cmd {
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => {
                assert_eq!(consultation_call_id, "consult-99");
            }
            _ => panic!("expected TransferCancel"),
        }
    }

    #[tokio::test]
    async fn test_transfer_cancel_succeeds_even_without_call() {
        let v = process_msg(
            r#"{"action": "call.transfer.cancel", "params": {"consultation_call_id": "nope"}}"#,
        )
        .await;
        assert_ne!(v["error"]["code"], "unknown_action");
    }
}
