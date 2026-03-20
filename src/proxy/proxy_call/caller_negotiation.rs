use crate::media::negotiate::{CodecInfo, MediaNegotiator};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use std::collections::HashSet;

fn append_dtmf_codecs(
    codec_info: &mut Vec<CodecInfo>,
    dtmf_codecs: &[CodecInfo],
    used_payload_types: &mut HashSet<u8>,
) {
    for dtmf in dtmf_codecs {
        if used_payload_types.insert(dtmf.payload_type) {
            codec_info.push(dtmf.clone());
        }
    }
}

pub(crate) fn should_advertise_caller_dtmf(
    use_media_proxy: bool,
    callee_has_dtmf: bool,
) -> bool {
    use_media_proxy || callee_has_dtmf
}

pub(crate) fn build_passthrough_caller_answer_codec_info(
    caller_offer_sdp: Option<&str>,
) -> Vec<CodecInfo> {
    let caller_codecs = caller_offer_sdp
        .map(MediaNegotiator::extract_codec_params)
        .unwrap_or_default();
    let mut codec_info = caller_codecs.audio;
    let mut used_payload_types = codec_info.iter().map(|codec| codec.payload_type).collect();
    append_dtmf_codecs(&mut codec_info, &caller_codecs.dtmf, &mut used_payload_types);
    codec_info
}

pub(crate) fn build_optimized_caller_codec_info(
    caller_offer_sdp: Option<&str>,
    callee_answer_sdp: &str,
    allow_codecs: &[CodecType],
    use_media_proxy: bool,
) -> Option<Vec<CodecInfo>> {
    let caller_offer_sdp = caller_offer_sdp?;
    let callee_extracted = MediaNegotiator::extract_codec_params(callee_answer_sdp);
    let selected_callee_codec =
        MediaNegotiator::select_best_codec(&callee_extracted.audio, allow_codecs)?;

    let caller_extracted = MediaNegotiator::extract_codec_params(caller_offer_sdp);
    let mut codec_info = caller_extracted.audio;
    if let Some(pos) = codec_info
        .iter()
        .position(|info| info.codec == selected_callee_codec.codec)
    {
        if pos > 0 {
            let preferred = codec_info.remove(pos);
            codec_info.insert(0, preferred);
        }
    }

    if should_advertise_caller_dtmf(use_media_proxy, !callee_extracted.dtmf.is_empty()) {
        let mut used_payload_types = codec_info.iter().map(|codec| codec.payload_type).collect();
        append_dtmf_codecs(
            &mut codec_info,
            &caller_extracted.dtmf,
            &mut used_payload_types,
        );
    }

    Some(codec_info)
}

pub(crate) fn build_final_caller_codec_info(
    caller_offer_sdp: Option<&str>,
    callee_answer_sdp: &str,
    allow_codecs: &[CodecType],
    use_media_proxy: bool,
) -> Result<Vec<CodecInfo>> {
    let caller_extracted = caller_offer_sdp
        .map(MediaNegotiator::extract_codec_params)
        .unwrap_or_default();
    let callee_extracted = MediaNegotiator::extract_codec_params(callee_answer_sdp);

    let mut negotiated_codecs = if allow_codecs.is_empty() {
        caller_extracted
            .audio
            .into_iter()
            .filter(|caller_codec| {
                callee_extracted
                    .audio
                    .iter()
                    .any(|callee_codec| callee_codec.codec == caller_codec.codec)
            })
            .collect()
    } else {
        let mut ordered = Vec::new();
        for codec_type in allow_codecs {
            if let Some(caller_codec) = caller_extracted
                .audio
                .iter()
                .find(|codec| codec.codec == *codec_type)
            {
                if callee_extracted
                    .audio
                    .iter()
                    .any(|codec| codec.codec == *codec_type)
                {
                    ordered.push(caller_codec.clone());
                }
            }
        }
        ordered
    };

    if negotiated_codecs.is_empty() {
        return Err(anyhow!(
            "No common codec found between caller, callee and dialplan"
        ));
    }

    if should_advertise_caller_dtmf(use_media_proxy, !callee_extracted.dtmf.is_empty()) {
        let mut used_payload_types = negotiated_codecs
            .iter()
            .map(|codec| codec.payload_type)
            .collect();
        append_dtmf_codecs(
            &mut negotiated_codecs,
            &caller_extracted.dtmf,
            &mut used_payload_types,
        );
    }

    Ok(negotiated_codecs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec_types(codec_info: &[CodecInfo]) -> Vec<CodecType> {
        codec_info.iter().map(|codec| codec.codec).collect()
    }

    #[test]
    fn test_should_advertise_caller_dtmf_for_media_proxy_even_without_callee_dtmf() {
        assert!(should_advertise_caller_dtmf(true, false));
        assert!(should_advertise_caller_dtmf(true, true));
        assert!(should_advertise_caller_dtmf(false, true));
        assert!(!should_advertise_caller_dtmf(false, false));
    }

    #[test]
    fn test_build_final_caller_codec_info_respects_dialplan_priority() {
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 111 9 0 8\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0 8 9\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:9 G722/8000\r\n";
        let codec_info = build_final_caller_codec_info(
            Some(caller_offer_sdp),
            callee_answer_sdp,
            &[CodecType::PCMA, CodecType::PCMU, CodecType::G722],
            false,
        )
        .unwrap();

        assert_eq!(
            codec_types(&codec_info),
            vec![CodecType::PCMA, CodecType::PCMU, CodecType::G722]
        );
    }

    #[test]
    fn test_build_final_caller_codec_info_uses_caller_order_when_dialplan_is_empty() {
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 0 8\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 8 0\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n";
        let codec_info =
            build_final_caller_codec_info(Some(caller_offer_sdp), callee_answer_sdp, &[], false)
                .unwrap();

        assert_eq!(codec_types(&codec_info), vec![CodecType::PCMU, CodecType::PCMA]);
    }

    #[test]
    fn test_build_final_caller_codec_info_preserves_dtmf_merge() {
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";
        let codec_info = build_final_caller_codec_info(
            Some(caller_offer_sdp),
            callee_answer_sdp,
            &[CodecType::PCMU],
            true,
        )
        .unwrap();

        assert_eq!(
            codec_types(&codec_info),
            vec![CodecType::PCMU, CodecType::TelephoneEvent]
        );
        assert_eq!(codec_info[1].payload_type, 101);
    }

    #[test]
    fn test_build_optimized_caller_codec_info_promotes_callee_selected_codec() {
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 96 0 8 101\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let codec_info = build_optimized_caller_codec_info(
            Some(caller_offer_sdp),
            callee_answer_sdp,
            &[CodecType::PCMU, CodecType::Opus],
            false,
        )
        .unwrap();

        assert_eq!(
            codec_types(&codec_info),
            vec![
                CodecType::PCMU,
                CodecType::Opus,
                CodecType::PCMA,
                CodecType::TelephoneEvent
            ]
        );
    }

    #[test]
    fn test_build_optimized_caller_codec_info_keeps_caller_order_when_callee_codec_is_unsupported()
    {
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 96 0 8 101\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 9\r\n\
            a=rtpmap:9 G722/8000\r\n";

        let codec_info = build_optimized_caller_codec_info(
            Some(caller_offer_sdp),
            callee_answer_sdp,
            &[CodecType::G722],
            false,
        )
        .unwrap();

        assert_eq!(
            codec_types(&codec_info),
            vec![
                CodecType::Opus,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent
            ]
        );
    }
}
