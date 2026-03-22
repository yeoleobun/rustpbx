use crate::call::app::ControllerEvent;
use crate::media::negotiate::CodecInfo;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub(crate) fn parse_dtmf_rfc4733(data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }

    let event = data[0];
    let end = (data[1] & 0x80) != 0;
    if !end {
        return None;
    }

    let digit = match event {
        0..=9 => char::from(b'0' + event),
        10 => '*',
        11 => '#',
        12 => 'A',
        13 => 'B',
        14 => 'C',
        15 => 'D',
        _ => return None,
    };

    Some(digit.to_string())
}

async fn dtmf_track_reader(
    track: Arc<dyn rustrtc::media::MediaStreamTrack>,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    dtmf_codecs: Vec<CodecInfo>,
    cancel: CancellationToken,
) {
    use rustrtc::media::MediaSample;

    let dtmf_payload_types: HashSet<u8> = dtmf_codecs
        .into_iter()
        .map(|codec| codec.payload_type)
        .collect();
    let mut last_event = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = track.recv() => {
                match result {
                    Ok(MediaSample::Audio(frame)) => {
                        if frame.payload_type.is_some_and(|pt| dtmf_payload_types.contains(&pt)) {
                            if let Some(digit) = parse_dtmf_rfc4733(&frame.data) {
                                if let Some((d, t)) = &last_event
                                    && d == &digit
                                    && *t == frame.rtp_timestamp
                                {
                                    continue;
                                }
                                last_event = Some((digit.clone(), frame.rtp_timestamp));
                                debug!(dtmf = %digit, "DTMF received from caller (RFC 4733)");
                                let _ = event_tx.send(ControllerEvent::DtmfReceived(digit));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

pub(crate) async fn spawn_caller_dtmf_listener(
    exported_peer: Arc<dyn MediaPeer>,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    dtmf_codecs: Vec<CodecInfo>,
    cancel: CancellationToken,
) {
    let mut started_track_ids = HashSet::new();
    let tracks = exported_peer.get_tracks().await;
    let Some(track_handle) = tracks.into_iter().next() else {
        return;
    };
    let pc = {
        let guard = track_handle.lock().await;
        guard.get_peer_connection().await
    };
    let Some(pc) = pc else { return };

    for transceiver in pc.get_transceivers() {
        if let Some(receiver) = transceiver.receiver() {
            let incoming_track = receiver.track();
            let track_id = incoming_track.id().to_string();
            if !started_track_ids.insert(track_id) {
                continue;
            }
            let tx = event_tx.clone();
            let codecs = dtmf_codecs.clone();
            let c = cancel.child_token();
            crate::utils::spawn(async move {
                dtmf_track_reader(incoming_track, tx, codecs, c).await;
            });
        }
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            event = pc.recv() => {
                if let Some(rustrtc::PeerConnectionEvent::Track(transceiver)) = event {
                    if let Some(receiver) = transceiver.receiver() {
                        let incoming_track = receiver.track();
                        let track_id = incoming_track.id().to_string();
                        if !started_track_ids.insert(track_id) {
                            continue;
                        }
                        let tx = event_tx.clone();
                        let codecs = dtmf_codecs.clone();
                        let c = cancel.clone();
                        tokio::spawn(async move {
                            dtmf_track_reader(incoming_track, tx, codecs, c).await;
                        });
                    }
                } else {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_dtmf_rfc4733;

    #[test]
    fn test_parse_dtmf_digits_0_to_9() {
        for code in 0u8..=9 {
            let payload = [code, 0x80, 0x00, 0xA0];
            assert_eq!(parse_dtmf_rfc4733(&payload), Some(code.to_string()));
        }
    }

    #[test]
    fn test_parse_dtmf_star_and_hash() {
        let star = [10u8, 0x80, 0x00, 0xA0];
        assert_eq!(parse_dtmf_rfc4733(&star), Some("*".to_string()));

        let hash = [11u8, 0x80, 0x00, 0xA0];
        assert_eq!(parse_dtmf_rfc4733(&hash), Some("#".to_string()));
    }

    #[test]
    fn test_parse_dtmf_abcd() {
        for (event, ch) in [(12u8, 'A'), (13u8, 'B'), (14u8, 'C'), (15u8, 'D')] {
            let payload = [event, 0x80, 0x00, 0xA0];
            assert_eq!(parse_dtmf_rfc4733(&payload), Some(ch.to_string()));
        }
    }

    #[test]
    fn test_parse_dtmf_no_end_bit_returns_none() {
        let payload = [1u8, 0x00, 0x00, 0xA0];
        assert_eq!(parse_dtmf_rfc4733(&payload), None);
    }

    #[test]
    fn test_parse_dtmf_short_payload_returns_none() {
        assert_eq!(parse_dtmf_rfc4733(&[]), None);
        assert_eq!(parse_dtmf_rfc4733(&[1, 0x80, 0x00]), None);
    }

    #[test]
    fn test_parse_dtmf_unknown_event_code_returns_none() {
        let payload = [16u8, 0x80, 0x00, 0xA0];
        assert_eq!(parse_dtmf_rfc4733(&payload), None);
    }
}
