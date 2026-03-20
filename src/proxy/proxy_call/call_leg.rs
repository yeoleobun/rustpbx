use crate::call::sip::DialogStateReceiverGuard;
use crate::media::negotiate::CodecInfo;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use audio_codec::CodecType;
use rsipstack::dialog::{DialogId, dialog::DialogState};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CallLegDirection {
    Inbound,
    Outbound,
}

pub(crate) struct CallLeg {
    #[allow(dead_code)]
    pub direction: CallLegDirection,
    pub peer: Arc<dyn MediaPeer>,
    pub offer_sdp: Option<String>,
    pub answer_sdp: Option<String>,
    pub negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    pub dialog_ids: Arc<Mutex<HashSet<DialogId>>>,
    pub connected_dialog_id: Option<DialogId>,
    pub dtmf_listener_cancel: Option<CancellationToken>,
    pub early_media_sent: bool,
    pub dialog_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    pub dialog_guards: Vec<DialogStateReceiverGuard>,
}

impl CallLeg {
    pub fn new(
        direction: CallLegDirection,
        peer: Arc<dyn MediaPeer>,
        offer_sdp: Option<String>,
    ) -> Self {
        Self {
            direction,
            peer,
            offer_sdp,
            answer_sdp: None,
            negotiated_audio: None,
            dialog_ids: Arc::new(Mutex::new(HashSet::new())),
            connected_dialog_id: None,
            dtmf_listener_cancel: None,
            early_media_sent: false,
            dialog_event_tx: None,
            dialog_guards: Vec::new(),
        }
    }
}
