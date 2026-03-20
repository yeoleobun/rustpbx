use crate::call::sip::DialogStateReceiverGuard;
use crate::call::Location;
use crate::proxy::proxy_call::call_leg::CallLegDirection;
use crate::proxy::proxy_call::session_timer::{
    HEADER_SESSION_EXPIRES, HEADER_SUPPORTED, SessionRefresher, SessionTimerState, TIMER_TAG, get_header_value,
    has_timer_support, parse_session_expires,
};
use anyhow::Result;
use rsip::StatusCode;
use rsipstack::dialog::{
    DialogId, dialog::Dialog, dialog::DialogState, dialog_layer::DialogLayer,
    invitation::InviteOption, server_dialog::ServerInviteDialog,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub(crate) const TRICKLE_ICE_TAG: &str = "trickle-ice";

pub(crate) struct SipLeg {
    #[allow(dead_code)]
    pub role: CallLegDirection,
    pub active_dialog_ids: Arc<Mutex<HashSet<DialogId>>>,
    pub recorded_dialog_ids: Arc<Mutex<HashSet<DialogId>>>,
    pub connected_dialog_id: Option<DialogId>,
    pub dialog_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    pub dialog_guards: Vec<DialogStateReceiverGuard>,
    pub session_timer: Arc<Mutex<SessionTimerState>>,
    pub supports_trickle_ice: bool,
}

impl SipLeg {
    pub fn new(role: CallLegDirection) -> Self {
        Self {
            role,
            active_dialog_ids: Arc::new(Mutex::new(HashSet::new())),
            recorded_dialog_ids: Arc::new(Mutex::new(HashSet::new())),
            connected_dialog_id: None,
            dialog_event_tx: None,
            dialog_guards: Vec::new(),
            session_timer: Arc::new(Mutex::new(SessionTimerState::default())),
            supports_trickle_ice: false,
        }
    }

    pub fn has_trickle_ice_support(headers: &rsip::Headers) -> bool {
        headers.iter().any(|h| match h {
            rsip::Header::Supported(s) => s
                .to_string()
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case(TRICKLE_ICE_TAG)),
            rsip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_SUPPORTED) => v
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case(TRICKLE_ICE_TAG)),
            rsip::Header::Other(n, v) if n.eq_ignore_ascii_case("X-RustPBX-Trickle-ICE") => {
                matches!(v.trim(), "1" | "true" | "yes")
            }
            _ => false,
        })
    }

    pub fn add_dialog(&self, dialog_id: DialogId) {
        {
            let mut dialogs = self.active_dialog_ids.lock().unwrap();
            dialogs.insert(dialog_id.clone());
        }
        let mut recorded = self.recorded_dialog_ids.lock().unwrap();
        recorded.insert(dialog_id);
    }

    pub fn recorded_dialogs(&self) -> Vec<DialogId> {
        self.recorded_dialog_ids
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    pub fn set_connected_dialog(&mut self, dialog_id: DialogId) {
        self.connected_dialog_id = Some(dialog_id);
    }

    #[allow(dead_code)]
    pub fn connected_dialog(&self) -> Option<&DialogId> {
        self.connected_dialog_id.as_ref()
    }

    pub fn local_contact_uri(
        routed_contact: Option<rsip::Uri>,
        server: &crate::proxy::server::SipServerInner,
    ) -> Option<rsip::Uri> {
        routed_contact.or_else(|| server.default_contact_uri())
    }

    pub fn build_outbound_invite_option(
        &self,
        target: &Location,
        caller: rsip::Uri,
        caller_display_name: Option<String>,
        offer: Option<Vec<u8>>,
        contact: Option<rsip::Uri>,
        headers: Vec<rsip::Header>,
        max_forwards: u32,
        session_timer: Option<u64>,
        call_id: Option<String>,
    ) -> InviteOption {
        let content_type = offer
            .as_ref()
            .map(|_| "application/sdp".to_string());
        let mut headers = headers;
        headers.push(rsip::headers::MaxForwards::from(max_forwards).into());
        if let Some(session_expires) = session_timer {
            headers.push(rsip::headers::Supported::from(TIMER_TAG).into());
            headers.push(rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                session_expires.to_string(),
            ));
        }
        InviteOption {
            caller_display_name,
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: contact.unwrap_or(caller),
            credential: target.credential.clone(),
            headers: Some(headers),
            call_id,
            ..Default::default()
        }
    }

    pub fn init_timer_from_initial_request(
        &self,
        request: &rsip::Request,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let headers = &request.headers;
        let supported = has_timer_support(headers);
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);
        let local_min_se = Duration::from_secs(90);
        let mut timer = self.session_timer.lock().unwrap();

        if let Some(value) = session_expires_value {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                if interval < local_min_se {
                    return Err((
                        StatusCode::SessionIntervalTooSmall,
                        Some(local_min_se.as_secs().to_string()),
                    ));
                }
                timer.enabled = true;
                timer.session_interval = interval;
                timer.active = true;
                timer.refresher = refresher.unwrap_or(SessionRefresher::Uac);
            }
        } else {
            timer.enabled = true;
            timer.session_interval = Duration::from_secs(default_expires);
            timer.active = true;
            timer.refresher = if supported {
                SessionRefresher::Uas
            } else {
                SessionRefresher::Uac
            };
        }
        Ok(())
    }

    pub fn init_timer_from_final_response(&self, response: &rsip::Response, default_expires: u64) {
        let headers = &response.headers;
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);
        let mut timer = self.session_timer.lock().unwrap();
        if let Some(value) = session_expires_value {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                timer.enabled = true;
                timer.session_interval = interval;
                timer.active = true;
                timer.last_refresh = Instant::now();
                timer.refresher = refresher.unwrap_or(SessionRefresher::Uac);
            }
        } else {
            timer.enabled = true;
            timer.session_interval = Duration::from_secs(default_expires);
            timer.active = true;
            timer.last_refresh = Instant::now();
            timer.refresher = SessionRefresher::Uac;
        }
    }

    pub fn refresh_timer_state(
        session_timer: &Arc<Mutex<SessionTimerState>>,
        headers: &rsip::Headers,
    ) {
        if let Some(value) = get_header_value(headers, HEADER_SESSION_EXPIRES) {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                let mut timer = session_timer.lock().unwrap();
                timer.session_interval = interval;
                timer.update_refresh();
                timer.active = true;
                if let Some(refresher) = refresher {
                    timer.refresher = refresher;
                }
                return;
            }
        }
        let mut timer = session_timer.lock().unwrap();
        if timer.active {
            timer.update_refresh();
        }
    }

    pub async fn hangup_inbound_dialog(
        &self,
        server_dialog: &ServerInviteDialog,
        code: Option<StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        if server_dialog.state().is_confirmed() || server_dialog.state().waiting_ack() {
            if let Some(reason) = reason {
                server_dialog.bye_with_reason(reason).await?;
            } else {
                server_dialog.bye().await?;
            }
        } else if !server_dialog.state().is_terminated() {
            let reject_code = code.unwrap_or(StatusCode::RequestTerminated);
            server_dialog.reject(Some(reject_code), reason)?;
        }
        Ok(())
    }

    pub async fn forward_remote_offer_to_client_dialog(
        &self,
        dialog_layer: Arc<DialogLayer>,
        method: rsip::Method,
        offer: String,
    ) {
        let Some(callee_dialog_id) = self.connected_dialog_id.as_ref() else {
            return;
        };
        let Some(dialog) = dialog_layer.get_dialog(callee_dialog_id) else {
            return;
        };
        if let Dialog::ClientInvite(client_dialog) = dialog {
            debug!(%callee_dialog_id, ?method, "Forwarding remote offer to client dialog");
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            let offer_body = Some(offer.into_bytes());
            if method == rsip::Method::Invite {
                let _ = client_dialog.reinvite(Some(headers), offer_body).await;
            } else {
                let _ = client_dialog.update(Some(headers), offer_body).await;
            }
        }
    }

    pub fn reply_to_server_reinvite(
        &self,
        server_dialog: &ServerInviteDialog,
        answer_sdp: Option<String>,
    ) -> bool {
        let Some(sdp) = answer_sdp else {
            let _ = server_dialog.reject(Some(StatusCode::NotAcceptableHere), None);
            return false;
        };
        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        match server_dialog.accept(Some(headers), Some(sdp.into_bytes())) {
            Ok(_) => true,
            Err(e) => {
                warn!("Failed to reply to re-INVITE with SDP: {}", e);
                false
            }
        }
    }

    pub fn reply_to_server_offerless_update(
        &self,
        server_dialog: &ServerInviteDialog,
        answer_sdp: Option<String>,
    ) {
        let Some(sdp) = answer_sdp else {
            return;
        };
        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        if let Err(e) = server_dialog.accept(Some(headers), Some(sdp.into_bytes())) {
            warn!("Failed to reply to UPDATE/re-INVITE without SDP: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_connected_dialog_tracks_value() {
        let mut sip_leg = SipLeg::new(CallLegDirection::Outbound);
        let dialog_id = DialogId {
            call_id: "call-id".to_string(),
            local_tag: "local-tag".to_string(),
            remote_tag: "remote-tag".to_string(),
        };
        sip_leg.set_connected_dialog(dialog_id.clone());
        assert_eq!(sip_leg.connected_dialog(), Some(&dialog_id));
    }

    #[test]
    fn test_init_timer_from_final_response_parses_header() {
        let sip_leg = SipLeg::new(CallLegDirection::Outbound);
        let response: rsip::Response = rsip::Response {
            status_code: rsip::StatusCode::OK,
            version: rsip::Version::V2,
            headers: vec![rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                "120;refresher=uac".into(),
            )]
            .into(),
            body: vec![],
        };
        sip_leg.init_timer_from_final_response(&response, 1800);
        let timer = sip_leg.session_timer.lock().unwrap().clone();
        assert!(timer.active);
        assert_eq!(timer.session_interval, Duration::from_secs(120));
        assert_eq!(timer.refresher, SessionRefresher::Uac);
    }
}
