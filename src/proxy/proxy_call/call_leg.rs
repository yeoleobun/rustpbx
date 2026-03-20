use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use crate::proxy::proxy_call::sip_leg::SipLeg;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CallLegDirection {
    Inbound,
    Outbound,
}

pub(crate) struct CallLeg {
    #[allow(dead_code)]
    pub direction: CallLegDirection,
    pub sip: SipLeg,
    pub media: MediaEndpoint,
}

impl CallLeg {
    pub fn new(
        direction: CallLegDirection,
        peer: std::sync::Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer>,
        offer_sdp: Option<String>,
    ) -> Self {
        Self {
            direction,
            sip: SipLeg::new(direction),
            media: MediaEndpoint::new(peer, offer_sdp),
        }
    }
}

impl Deref for CallLeg {
    type Target = MediaEndpoint;

    fn deref(&self) -> &Self::Target {
        &self.media
    }
}

impl DerefMut for CallLeg {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.media
    }
}
