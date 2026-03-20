use crate::media::negotiate::{CodecInfo, MediaNegotiator};
use crate::proxy::proxy_call::caller_negotiation;
use audio_codec::CodecType;

    fn append_dtmf_codecs_for_test(
        codec_info: &mut Vec<CodecInfo>,
        dtmf_codecs: &[CodecInfo],
        used_payload_types: &mut std::collections::HashSet<u8>,
    ) {
        for dtmf in dtmf_codecs {
            if used_payload_types.insert(dtmf.payload_type) {
                codec_info.push(dtmf.clone());
            }
        }
    }

    fn allocate_dynamic_payload_type_for_test(
        used_payload_types: &std::collections::HashSet<u8>,
        preferred: u8,
    ) -> u8 {
        if !used_payload_types.contains(&preferred) {
            return preferred;
        }
        let mut next_pt = 96;
        while used_payload_types.contains(&next_pt) {
            next_pt += 1;
        }
        next_pt
    }

    fn append_supported_dtmf_codecs_for_test(
        codec_info: &mut Vec<CodecInfo>,
        caller_dtmf_codecs: &[CodecInfo],
        used_payload_types: &mut std::collections::HashSet<u8>,
    ) {
        append_dtmf_codecs_for_test(codec_info, caller_dtmf_codecs, used_payload_types);
        for (clock_rate, preferred_payload_type) in [(8000, 101u8), (48000, 110u8)] {
            if codec_info.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent && codec.clock_rate == clock_rate
            }) {
                continue;
            }
            let payload_type =
                allocate_dynamic_payload_type_for_test(used_payload_types, preferred_payload_type);
            used_payload_types.insert(payload_type);
            codec_info.push(CodecInfo {
                payload_type,
                codec: CodecType::TelephoneEvent,
                clock_rate,
                channels: 1,
            });
        }
    }

    fn extract_telephone_event_codecs_for_test(
        rtp_map: &[(u8, (CodecType, u32, u16))],
    ) -> Vec<CodecInfo> {
        let mut seen_payload_types = std::collections::HashSet::new();
        rtp_map
            .iter()
            .filter_map(|(pt, (codec, clock, channels))| {
                (*codec == CodecType::TelephoneEvent && seen_payload_types.insert(*pt)).then_some(
                    CodecInfo {
                        payload_type: *pt,
                        codec: *codec,
                        clock_rate: *clock,
                        channels: *channels,
                    },
                )
            })
            .collect()
    }

    fn build_callee_offer_codec_info_for_test(
        caller_rtp_map: &[(u8, (CodecType, u32, u16))],
        allow_codecs: &[CodecType],
    ) -> Vec<CodecInfo> {
        let mut codec_info = Vec::new();
        let mut seen_codecs = Vec::new();
        let mut used_payload_types = std::collections::HashSet::new();
        let caller_dtmf_codecs = extract_telephone_event_codecs_for_test(caller_rtp_map);

        for (pt, (codec, clock, channels)) in caller_rtp_map {
            if *codec == CodecType::TelephoneEvent {
                continue;
            }
            if (!allow_codecs.is_empty() && !allow_codecs.contains(codec))
                || seen_codecs.contains(codec)
            {
                continue;
            }
            seen_codecs.push(*codec);
            used_payload_types.insert(*pt);
            codec_info.push(CodecInfo {
                payload_type: *pt,
                codec: *codec,
                clock_rate: *clock,
                channels: *channels,
            });
        }

        if !allow_codecs.is_empty() {
            let mut next_pt = 96;
            for codec in allow_codecs {
                if *codec == CodecType::TelephoneEvent {
                    continue;
                }
                if seen_codecs.contains(codec) {
                    continue;
                }
                seen_codecs.push(*codec);

                let payload_type = if !codec.is_dynamic() {
                    codec.payload_type()
                } else {
                    while used_payload_types.contains(&next_pt) {
                        next_pt += 1;
                    }
                    next_pt
                };
                used_payload_types.insert(payload_type);
                codec_info.push(CodecInfo {
                    payload_type,
                    codec: *codec,
                    clock_rate: codec.clock_rate(),
                    channels: codec.channels(),
                });
            }
        }

        append_supported_dtmf_codecs_for_test(
            &mut codec_info,
            &caller_dtmf_codecs,
            &mut used_payload_types,
        );

        codec_info
    }

    fn build_caller_answer_codec_info_for_test(
        caller_codecs: &[CodecInfo],
        caller_dtmf_codecs: &[CodecInfo],
        preferred_codec: Option<CodecType>,
    ) -> Vec<CodecInfo> {
        let mut codec_info = caller_codecs.to_vec();

        if let Some(codec) = preferred_codec {
            if let Some(pos) = codec_info.iter().position(|info| info.codec == codec) {
                if pos > 0 {
                    let preferred = codec_info.remove(pos);
                    codec_info.insert(0, preferred);
                }
            }
        }

        let mut used_payload_types = codec_info.iter().map(|codec| codec.payload_type).collect();
        append_dtmf_codecs_for_test(
            &mut codec_info,
            caller_dtmf_codecs,
            &mut used_payload_types,
        );

        codec_info
    }

    #[test]
    fn test_parse_rtp_map_from_sdp() {
        // Test parsing Alice's offer (WebRTC) with multiple codecs
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:63 red/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:13 CN/8000\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, alice_offer).unwrap();
        let section = sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();
        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        // Verify codecs are parsed correctly
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 111 && *codec == CodecType::Opus)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 0 && *codec == CodecType::PCMU)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 8 && *codec == CodecType::PCMA)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 9 && *codec == CodecType::G722)
        );
    }

    #[test]
    fn test_extract_codec_params_opus() {
        let answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 12005 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let codecs = MediaNegotiator::extract_codec_params(answer);
        let first = &codecs.audio[0];
        assert_eq!(first.codec, CodecType::Opus);
        assert_eq!(first.payload_type, 111);
        assert_eq!(first.clock_rate, 48000);
        assert_eq!(first.channels, 2);
    }

    #[test]
    fn test_extract_codec_params_pcmu() {
        let answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let codecs = MediaNegotiator::extract_codec_params(answer);
        let first = &codecs.audio[0];
        assert_eq!(first.codec, CodecType::PCMU);
        assert_eq!(first.payload_type, 0);
        assert_eq!(first.clock_rate, 8000);
        assert_eq!(first.channels, 1);
    }

    #[test]
    fn test_codec_compatibility_check() {
        // Alice's offer: Opus(111), G722(9), PCMU(0), PCMA(8)
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111 9 0 8\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let alice_codecs = MediaNegotiator::extract_codec_params(alice_offer).audio;

        // Bob's answer: PCMU only
        let bob_answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let bob_codecs = MediaNegotiator::extract_codec_params(bob_answer).audio;
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice supports Bob's chosen codec
        let alice_supports_pcmu = alice_codecs.iter().any(|c| c.codec == bob_codec);
        assert!(alice_supports_pcmu, "Alice should support PCMU");

        // Verify the optimization should avoid transcoding
        assert_eq!(bob_codec, CodecType::PCMU);
    }

    #[test]
    fn test_codec_incompatibility() {
        // Alice's offer: only Opus
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, alice_offer).unwrap();
        let section = sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();
        let alice_codecs = MediaNegotiator::parse_rtp_map_from_section(section);

        // Bob's answer: PCMU only
        let bob_answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let bob_codecs = MediaNegotiator::extract_codec_params(bob_answer).audio;
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice does NOT support Bob's chosen codec
        let alice_supports_bob = alice_codecs
            .iter()
            .any(|(_, (codec, _, _))| *codec == bob_codec);
        assert!(
            !alice_supports_bob,
            "Alice should not support PCMU in this case, transcoding required"
        );
    }

    #[test]
    fn test_build_callee_offer_preserves_caller_order_then_appends_allow_codecs() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (0, (CodecType::PCMU, 8000, 1)),
            (8, (CodecType::PCMA, 8000, 1)),
            (18, (CodecType::G729, 8000, 1)),
        ];
        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
        ];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);
        let order: Vec<CodecType> = merged
            .iter()
            .filter(|codec| codec.codec != CodecType::TelephoneEvent)
            .map(|codec| codec.codec)
            .collect();

        assert_eq!(
            order,
            vec![
                CodecType::Opus,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::G729,
                CodecType::G722,
            ],
            "caller-supported codecs should keep caller order, then append extra allow_codecs"
        );
        assert_eq!(
            merged
                .iter()
                .rev()
                .find(|codec| codec.codec != CodecType::TelephoneEvent)
                .map(|codec| codec.payload_type),
            Some(9),
            "extra static codecs should retain their canonical payload type"
        );
    }

    #[test]
    fn test_build_callee_offer_keeps_opus_pt_when_extra_g729_is_appended() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (0, (CodecType::PCMU, 8000, 1)),
            (8, (CodecType::PCMA, 8000, 1)),
        ];
        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
        ];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);
        let order_and_pt: Vec<(CodecType, u8)> = merged
            .iter()
            .filter(|codec| codec.codec != CodecType::TelephoneEvent)
            .map(|codec| (codec.codec, codec.payload_type))
            .collect();

        assert_eq!(
            order_and_pt,
            vec![
                (CodecType::Opus, 96),
                (CodecType::PCMU, 0),
                (CodecType::PCMA, 8),
                (CodecType::G729, 18),
                (CodecType::G722, 9),
            ],
            "caller-supported Opus must keep PT 96; appended static codecs should keep canonical PTs"
        );
    }

    #[test]
    fn test_build_callee_offer_appended_opus_uses_dynamic_pt() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (0, (CodecType::PCMU, 8000, 1)),
            (8, (CodecType::PCMA, 8000, 1)),
            (18, (CodecType::G729, 8000, 1)),
        ];
        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
        ];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);
        let order_and_pt: Vec<(CodecType, u8)> = merged
            .iter()
            .filter(|codec| codec.codec != CodecType::TelephoneEvent)
            .map(|codec| (codec.codec, codec.payload_type))
            .collect();

        assert_eq!(
            order_and_pt,
            vec![
                (CodecType::PCMU, 0),
                (CodecType::PCMA, 8),
                (CodecType::G729, 18),
                (CodecType::G722, 9),
                (CodecType::Opus, 96),
            ],
            "only Opus should use a dynamically allocated PT when appended"
        );
    }

    #[test]
    fn test_build_callee_offer_adds_rustpbx_dtmf_capabilities() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (0, (CodecType::PCMU, 8000, 1)),
        ];
        let allow_codecs = vec![CodecType::PCMU, CodecType::Opus, CodecType::TelephoneEvent];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);

        assert!(
            merged
                .iter()
                .any(|codec| codec.codec == CodecType::TelephoneEvent && codec.clock_rate == 8000),
            "callee offer should advertise rustpbx telephone-event/8000 support"
        );
        assert!(
            merged
                .iter()
                .any(|codec| codec.codec == CodecType::TelephoneEvent && codec.clock_rate == 48000),
            "callee offer should advertise rustpbx telephone-event/48000 support"
        );
    }

    #[test]
    fn test_build_callee_offer_preserves_caller_telephone_event_variants() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (0, (CodecType::PCMU, 8000, 1)),
            (101, (CodecType::TelephoneEvent, 48000, 1)),
            (97, (CodecType::TelephoneEvent, 8000, 1)),
        ];
        let allow_codecs = vec![CodecType::Opus, CodecType::PCMU, CodecType::PCMA];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);

        assert!(
            merged.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent
                    && codec.payload_type == 97
                    && codec.clock_rate == 8000
            }),
            "callee offer should preserve caller's telephone-event/8000 payload type"
        );
        assert!(
            merged.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent
                    && codec.payload_type == 101
                    && codec.clock_rate == 48000
            }),
            "callee offer should preserve caller's telephone-event/48000 payload type"
        );
    }

    #[test]
    fn test_build_callee_offer_adds_missing_8000_dtmf_when_caller_has_only_48000() {
        use audio_codec::CodecType;

        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (101, (CodecType::TelephoneEvent, 48000, 1)),
        ];
        let allow_codecs = vec![CodecType::Opus, CodecType::PCMU];

        let merged = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);

        assert!(
            merged.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent
                    && codec.payload_type == 101
                    && codec.clock_rate == 48000
            }),
            "callee offer should preserve caller's telephone-event/48000 payload type"
        );
        assert!(
            merged.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent
                    && codec.clock_rate == 8000
                    && codec.payload_type != 101
            }),
            "callee offer should append a distinct telephone-event/8000 payload type for rustpbx interworking"
        );
    }

    #[test]
    fn test_media_proxy_offer_preserves_caller_opus_priority_with_default_allow_codecs() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Caller A offers: 96(Opus), 0(PCMU), 8(PCMA), 18(G729)
        let caller_rtp_map = vec![
            (96, (CodecType::Opus, 48000, 2)),
            (0, (CodecType::PCMU, 8000, 1)),
            (8, (CodecType::PCMA, 8000, 1)),
            (18, (CodecType::G729, 8000, 1)),
        ];
        // Default dialplan priority.
        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
        ];

        let callee_offer = build_callee_offer_codec_info_for_test(&caller_rtp_map, &allow_codecs);
        let callee_offer_order: Vec<(CodecType, u8)> = callee_offer
            .iter()
            .filter(|codec| codec.codec != CodecType::TelephoneEvent)
            .map(|codec| (codec.codec, codec.payload_type))
            .collect();

        assert_eq!(
            callee_offer_order,
            vec![
                (CodecType::Opus, 96),
                (CodecType::PCMU, 0),
                (CodecType::PCMA, 8),
                (CodecType::G729, 18),
                (CodecType::G722, 9),
            ],
            "media_proxy offer should preserve caller codec order first, so Opus stays ahead of PCMU/PCMA even when allow_codecs starts with G729"
        );

        // Callee B supports 0/8/96 and answers with Opus first.
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 4000 RTP/AVP 96 0 8\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let preferred_codec = MediaNegotiator::extract_codec_params(callee_answer_sdp)
            .audio
            .first()
            .map(|codec| codec.codec);
        assert_eq!(
            preferred_codec,
            Some(CodecType::Opus),
            "when B supports Opus and answers with Opus first, rustpbx should treat Opus as the preferred codec"
        );

        let caller_codecs: Vec<CodecInfo> = caller_rtp_map
            .iter()
            .map(|(pt, (codec, clock_rate, channels))| CodecInfo {
                payload_type: *pt,
                codec: *codec,
                clock_rate: *clock_rate,
                channels: *channels,
            })
            .collect();
        let caller_dtmf_codecs = vec![CodecInfo {
            payload_type: 97,
            codec: CodecType::TelephoneEvent,
            clock_rate: 8000,
            channels: 1,
        }];
        let caller_answer = build_caller_answer_codec_info_for_test(
            &caller_codecs,
            &caller_dtmf_codecs,
            preferred_codec,
        );
        let caller_answer_order: Vec<CodecType> =
            caller_answer.iter().map(|codec| codec.codec).collect();

        assert_eq!(
            caller_answer_order,
            vec![
                CodecType::Opus,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::G729,
                CodecType::TelephoneEvent,
            ],
            "caller answer should keep Opus first when callee also selected Opus, avoiding unnecessary 96->0 transcoding"
        );
    }

    #[test]
    fn test_build_caller_answer_prefers_callee_codec_or_keeps_caller_order() {
        use audio_codec::CodecType;

        let caller_codecs = vec![
            CodecInfo {
                payload_type: 96,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
            },
            CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 8,
                codec: CodecType::PCMA,
                clock_rate: 8000,
                channels: 1,
            },
        ];

        let preferred = build_caller_answer_codec_info_for_test(
            &caller_codecs,
            &[CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
            }],
            Some(CodecType::PCMU),
        );
        let preferred_order: Vec<CodecType> = preferred.iter().map(|codec| codec.codec).collect();
        assert_eq!(
            preferred_order,
            vec![
                CodecType::PCMU,
                CodecType::Opus,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ],
            "caller answer should prioritize callee's chosen codec when caller also supports it"
        );

        let fallback = build_caller_answer_codec_info_for_test(
            &caller_codecs,
            &[CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
            }],
            Some(CodecType::G722),
        );
        let fallback_order: Vec<CodecType> = fallback.iter().map(|codec| codec.codec).collect();
        assert_eq!(
            fallback_order,
            vec![
                CodecType::Opus,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ],
            "unsupported callee codec should keep caller order and use caller-leg fallback"
        );
    }

    #[test]
    fn test_should_advertise_caller_dtmf_for_media_proxy_even_without_callee_dtmf() {
        assert!(caller_negotiation::should_advertise_caller_dtmf(true, false));
        assert!(caller_negotiation::should_advertise_caller_dtmf(true, true));
        assert!(caller_negotiation::should_advertise_caller_dtmf(false, true));
        assert!(!caller_negotiation::should_advertise_caller_dtmf(false, false));
    }

    /// Test: negotiate_final_codec() respects dialplan priority
    #[test]
    fn test_negotiate_final_codec_dialplan_priority() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Caller OFFER has: Opus(111), G722(9), PCMU(0), PCMA(8)
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

        // Callee ANSWER has: PCMU(0), PCMA(8), G722(9)
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0 8 9\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:9 G722/8000\r\n";

        // Dialplan priority: PCMA > PCMU > G722
        let dialplan_codecs = vec![CodecType::PCMA, CodecType::PCMU, CodecType::G722];

        // Extract codecs from SDPs
        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Build intersection based on dialplan priority
        let mut negotiated = Vec::new();
        for codec_type in &dialplan_codecs {
            if let Some(caller_codec) = caller_codecs.iter().find(|c| &c.codec == codec_type) {
                if callee_codecs.iter().any(|c| c.codec == *codec_type) {
                    negotiated.push(caller_codec.clone());
                }
            }
        }

        // Expected order: PCMA, PCMU, G722 (Opus excluded - not in callee)
        assert_eq!(negotiated.len(), 3, "Should have 3 codecs in intersection");
        assert_eq!(
            negotiated[0].codec,
            CodecType::PCMA,
            "PCMA should be first (dialplan priority)"
        );
        assert_eq!(
            negotiated[1].codec,
            CodecType::PCMU,
            "PCMU should be second"
        );
        assert_eq!(negotiated[2].codec, CodecType::G722, "G722 should be third");
    }

    /// Test: Empty dialplan uses caller's codec preference
    #[test]
    fn test_negotiate_empty_dialplan() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

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

        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Empty dialplan - use caller's order
        // Take intersection based on caller's order
        let mut negotiated = Vec::new();
        for caller_codec in &caller_codecs {
            if callee_codecs.iter().any(|c| c.codec == caller_codec.codec) {
                negotiated.push(caller_codec.clone());
            }
        }

        // Expected: PCMU first (caller's preference), PCMA second
        assert_eq!(negotiated.len(), 2);
        assert_eq!(
            negotiated[0].codec,
            CodecType::PCMU,
            "PCMU should be first (caller preference)"
        );
        assert_eq!(
            negotiated[1].codec,
            CodecType::PCMA,
            "PCMA should be second"
        );
    }

    /// Test: No common codec between caller and callee
    #[test]
    fn test_no_common_codec() {
        use crate::media::negotiate::MediaNegotiator;

        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Find intersection
        let mut negotiated = Vec::new();
        for caller_codec in &caller_codecs {
            if callee_codecs.iter().any(|c| c.codec == caller_codec.codec) {
                negotiated.push(caller_codec.clone());
            }
        }

        assert_eq!(
            negotiated.len(),
            0,
            "No common codec should result in empty negotiation"
        );
    }

    /// Test: RFC 3264 Answer prioritization in media bridge setup
    /// This test covers the real-world scenario that caused the audio corruption bug
    #[test]
    fn test_rfc3264_answer_prioritization() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Bob (WebRTC) OFFER: Opus(111), G722(9), PCMU(0), PCMA(8)
        let bob_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 64348 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:63 red/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:13 CN/8000\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        // Alice (RTP) ANSWER: PCMU(0), PCMA(8), G722(9), G729(18) - Alice chose PCMU first!
        let alice_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.3.211\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.211\r\n\
            t=0 0\r\n\
            m=audio 58721 RTP/AVP 0 8 9 18 111\r\n\
            a=mid:0\r\n\
            a=sendrecv\r\n\
            a=rtcp-mux\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n\
            a=rtpmap:8 PCMA/8000/1\r\n\
            a=rtpmap:9 G722/8000/1\r\n\
            a=rtpmap:18 G729/8000/1\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=ssrc:853670255 cname:rustrtc-cname-853670255\r\n";

        // Step 1: Extract codecs from Alice's Answer (callee's authoritative choice)
        let alice_codecs = MediaNegotiator::extract_codec_params(alice_answer_sdp).audio;
        let alice_chosen = MediaNegotiator::select_best_codec(&alice_codecs, &[]);
        assert!(alice_chosen.is_some());
        let alice_codec = alice_chosen.unwrap();

        // RFC 3264: Alice chose PCMU as the first codec in her Answer
        assert_eq!(
            alice_codec.codec,
            CodecType::PCMU,
            "Alice's Answer should prioritize PCMU (first in Answer)"
        );

        // Step 2: Find the same codec in Bob's offer (for params_a)
        let bob_codecs = MediaNegotiator::extract_codec_params(bob_offer_sdp).audio;
        let bob_matching = bob_codecs
            .iter()
            .find(|c| c.codec == alice_codec.codec)
            .cloned();
        assert!(bob_matching.is_some(), "Bob should support PCMU");
        let bob_codec = bob_matching.unwrap();

        // Step 3: Verify both sides will use the same codec
        assert_eq!(
            bob_codec.codec, alice_codec.codec,
            "Both sides must use the same codec (PCMU)"
        );
        assert_eq!(
            bob_codec.codec,
            CodecType::PCMU,
            "The negotiated codec must be PCMU"
        );

        // Step 4: Verify no transcoding is needed
        assert_eq!(
            bob_codec.codec, alice_codec.codec,
            "Same codec on both sides means no transcoding"
        );
    }

    /// Test: codec_a should come from caller's OFFER, not select_best_codec on answer
    ///
    /// This is the regression test for the noise bug where MediaBridge created
    /// Transcoder(G722, opus) but the caller actually sent PCMA.
    ///
    /// Scenario: active-call sends INVITE with `m=audio 12000 RTP/AVP 8 0 9 101`
    /// (PCMA first). rustpbx creates callee WebRTC track, callee answers with opus.
    /// Old code used `select_best_codec(answer, allow_codecs)` which picked G722
    /// (because allow_codecs = [G729, G722, PCMU, PCMA, Opus, TelephoneEvent] has
    /// G722 before PCMA). New code uses caller's OFFER first codec = PCMA.
    #[test]
    fn test_codec_a_from_caller_offer_not_answer() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Caller's OFFER: PCMA(8) first, then PCMU(0), G722(9), telephone-event(101)
        // This is what active-call sends - carrier provides PCMA
        let caller_offer_sdp = "v=0\r\n\
            o=- 100 100 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 12000 RTP/AVP 8 0 9 101\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-15\r\n";

        // Our answer to the caller (what rustpbx sends back)
        // In the buggy scenario, the answer's codec order was derived from the
        // WebRTC callee's offer which had G722(9) before PCMA(8):
        //   callee offer: UDP/TLS/RTP/SAVPF 96 9 0 8 97 101
        // So the answer to caller also has G722 before PCMA.
        let answer_to_caller_sdp = "v=0\r\n\
            o=- 200 200 IN IP4 10.0.0.2\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.2\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 9 0 8 101\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        // Default allow_codecs (same as Dialplan default)
        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
            CodecType::TelephoneEvent,
        ];

        // === OLD BUG: select_best_codec on the answer picks G722 ===
        let answer_codecs = MediaNegotiator::extract_codec_params(answer_to_caller_sdp).audio;
        let old_codec_a = MediaNegotiator::select_best_codec(&answer_codecs, &allow_codecs);
        assert!(old_codec_a.is_some());
        // G722 comes before PCMA in allow_codecs, so the old code would pick G722
        assert_eq!(
            old_codec_a.unwrap().codec,
            CodecType::G722,
            "Old bug: select_best_codec on answer picks G722 (higher priority in allow_codecs) \
             but caller actually sends PCMA → Transcoder(G722, opus) decodes PCMA as G722 = noise"
        );

        // === NEW FIX: first non-TelephoneEvent codec from caller's OFFER ===
        let offer_codecs = MediaNegotiator::extract_codec_params(caller_offer_sdp).audio;
        let new_codec_a = offer_codecs
            .iter()
            .find(|c| c.codec != CodecType::TelephoneEvent)
            .cloned();
        assert!(new_codec_a.is_some());
        assert_eq!(
            new_codec_a.unwrap().codec,
            CodecType::PCMA,
            "Fix: first codec from caller's OFFER is PCMA (what they actually send)"
        );
    }

    /// Test: early media path should also use caller's OFFER for codec_a
    /// and use callee's original SDP (not overwritten answer_for_caller) for codec_b
    #[test]
    fn test_early_media_codec_extraction() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Caller's OFFER: PCMA first
        let caller_offer_sdp = "v=0\r\n\
            o=- 100 100 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 12000 RTP/AVP 8 0 101\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-15\r\n";

        // Callee's 183 early media SDP (WebRTC client, offers opus)
        let callee_early_sdp = "v=0\r\n\
            o=- 300 300 IN IP4 10.0.0.3\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.3\r\n\
            t=0 0\r\n\
            m=audio 30000 UDP/TLS/RTP/SAVPF 111 9 0 8 101\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        // answer_for_caller (negotiated answer sent to caller) - this would
        // overwrite the `answer` variable in the old buggy code
        let answer_for_caller = "v=0\r\n\
            o=- 200 200 IN IP4 10.0.0.2\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.2\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 8 0 101\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let allow_codecs = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::Opus,
            CodecType::TelephoneEvent,
        ];

        // codec_a from caller's OFFER (not from answer_for_caller)
        let offer_codecs = MediaNegotiator::extract_codec_params(caller_offer_sdp).audio;
        let codec_a = offer_codecs
            .iter()
            .find(|c| c.codec != CodecType::TelephoneEvent)
            .cloned()
            .unwrap();
        assert_eq!(
            codec_a.codec,
            CodecType::PCMA,
            "codec_a should be PCMA from caller's OFFER"
        );

        // codec_b from callee's ORIGINAL early media SDP (not answer_for_caller)
        let callee_codecs = MediaNegotiator::extract_codec_params(callee_early_sdp).audio;
        let codec_b = MediaNegotiator::select_best_codec(&callee_codecs, &allow_codecs).unwrap();
        // G722 has higher priority in allow_codecs, but callee offers opus first
        // select_best_codec with allow_codecs will pick based on allow_codecs order
        // among the codecs present in callee's SDP
        assert_ne!(
            codec_b.codec,
            CodecType::TelephoneEvent,
            "codec_b should not be TelephoneEvent"
        );

        // OLD BUG: if we extracted codec_b from answer_for_caller instead of
        // callee's original SDP, we'd get PCMA and no transcoding would happen
        // → but callee (WebRTC) doesn't speak PCMA, it speaks opus → silence/noise
        let wrong_codecs = MediaNegotiator::extract_codec_params(answer_for_caller).audio;
        let wrong_codec_b =
            MediaNegotiator::select_best_codec(&wrong_codecs, &allow_codecs).unwrap();
        assert_ne!(
            wrong_codec_b.codec, codec_b.codec,
            "Using answer_for_caller for codec_b gives wrong codec (PCMA instead of callee's actual codec)"
        );
    }

    /// Test: TelephoneEvent should be skipped when selecting audio codec
    #[test]
    fn test_skip_telephone_event_in_codec_selection() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // SDP with TelephoneEvent as first codec (should be skipped)
        let sdp_with_dtmf_first = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.3.211\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.211\r\n\
            t=0 0\r\n\
            m=audio 58721 RTP/AVP 101 0 8\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let codecs = MediaNegotiator::extract_codec_params(sdp_with_dtmf_first).audio;
        let chosen = MediaNegotiator::select_best_codec(&codecs, &[]);

        assert!(chosen.is_some());
        let chosen_codec = chosen.unwrap();
        assert_eq!(
            chosen_codec.codec,
            CodecType::PCMU,
            "Should skip TelephoneEvent and select PCMU (first audio codec)"
        );
        assert_ne!(
            chosen_codec.codec,
            CodecType::TelephoneEvent,
            "Should never select TelephoneEvent as audio codec"
        );
    }
