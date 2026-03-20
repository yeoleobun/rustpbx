use crate::call::MediaConfig;
use crate::call::app::ControllerEvent;
use crate::media::FileTrack;
use crate::media::RtpTrackBuilder;
use crate::media::Track;
use crate::media::negotiate::MediaNegotiator;
use crate::media::negotiate::CodecInfo;
use crate::media::recorder::RecorderOption;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use audio_codec::CodecType;
use anyhow::{Result, anyhow};
use rustrtc::SessionDescription;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use std::collections::HashSet;

#[derive(Clone, Debug)]
pub(crate) struct BridgeSelection {
    pub codec: CodecType,
    pub params: rustrtc::RtpCodecParameters,
    pub dtmf_codecs: Vec<CodecInfo>,
    pub ssrc: Option<u32>,
}

impl BridgeSelection {
    pub fn from_audio_tuple(
        selection: (CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>),
        ssrc: Option<u32>,
    ) -> Self {
        let (codec, params, dtmf_codecs) = selection;
        Self {
            codec,
            params,
            dtmf_codecs,
            ssrc,
        }
    }
}

pub(crate) struct MediaEndpoint {
    pub peer: Arc<dyn MediaPeer>,
    pub offer_sdp: Option<String>,
    pub answer_sdp: Option<String>,
    pub negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    #[allow(dead_code)]
    pub ssrc: Option<u32>,
    pub dtmf_listener_cancel: Option<CancellationToken>,
    pub early_media_sent: bool,
    #[allow(dead_code)]
    pub directional_recorder_option: Option<RecorderOption>,
}

impl MediaEndpoint {
    pub fn new(peer: Arc<dyn MediaPeer>, offer_sdp: Option<String>) -> Self {
        Self {
            peer,
            offer_sdp,
            answer_sdp: None,
            negotiated_audio: None,
            ssrc: None,
            dtmf_listener_cancel: None,
            early_media_sent: false,
            directional_recorder_option: None,
        }
    }

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

    fn allocate_dynamic_payload_type(used_payload_types: &HashSet<u8>, preferred: u8) -> u8 {
        if !used_payload_types.contains(&preferred) {
            return preferred;
        }
        let mut next_pt = 96;
        while used_payload_types.contains(&next_pt) {
            next_pt += 1;
        }
        next_pt
    }

    fn append_supported_dtmf_codecs(
        codec_info: &mut Vec<CodecInfo>,
        caller_dtmf_codecs: &[CodecInfo],
        used_payload_types: &mut HashSet<u8>,
    ) {
        Self::append_dtmf_codecs(codec_info, caller_dtmf_codecs, used_payload_types);
        for (clock_rate, preferred_payload_type) in [(8000, 101u8), (48000, 110u8)] {
            if codec_info.iter().any(|codec| {
                codec.codec == CodecType::TelephoneEvent && codec.clock_rate == clock_rate
            }) {
                continue;
            }
            let payload_type =
                Self::allocate_dynamic_payload_type(used_payload_types, preferred_payload_type);
            used_payload_types.insert(payload_type);
            codec_info.push(CodecInfo {
                payload_type,
                codec: CodecType::TelephoneEvent,
                clock_rate,
                channels: 1,
            });
        }
    }

    fn extract_telephone_event_codecs(
        rtp_map: &[(u8, (CodecType, u32, u16))],
    ) -> Vec<CodecInfo> {
        let mut seen_payload_types = HashSet::new();
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

    fn build_local_offer_codec_info_from_rtp_map(
        caller_rtp_map: &[(u8, (CodecType, u32, u16))],
        allow_codecs: &[CodecType],
    ) -> Vec<CodecInfo> {
        let mut codec_info = Vec::new();
        let mut seen_codecs = Vec::new();
        let mut used_payload_types = HashSet::new();
        let caller_dtmf_codecs = Self::extract_telephone_event_codecs(caller_rtp_map);

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
                if *codec == CodecType::TelephoneEvent || seen_codecs.contains(codec) {
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

        Self::append_supported_dtmf_codecs(
            &mut codec_info,
            &caller_dtmf_codecs,
            &mut used_payload_types,
        );

        codec_info
    }

    pub fn select_best_audio_from_sdp(
        sdp: &str,
        allow_codecs: &[CodecType],
    ) -> Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)> {
        let extracted = MediaNegotiator::extract_codec_params(sdp);
        let dtmf = extracted.dtmf.clone();
        MediaNegotiator::select_best_codec(&extracted.audio, allow_codecs)
            .map(|codec| (codec.codec, codec.to_params(), dtmf))
    }

    pub fn bridge_selection_from_sdp(
        sdp: &str,
        allow_codecs: &[CodecType],
    ) -> Option<BridgeSelection> {
        Self::select_best_audio_from_sdp(sdp, allow_codecs)
            .map(|selection| BridgeSelection::from_audio_tuple(selection, Self::ssrc_from_sdp(sdp)))
    }

    fn preferred_audio_from_sdp(
        sdp: &str,
        preferred_codec: Option<CodecType>,
    ) -> Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)> {
        let extracted = MediaNegotiator::extract_codec_params(sdp);
        let chosen = preferred_codec
            .and_then(|preferred| {
                extracted
                    .audio
                    .iter()
                    .find(|info| info.codec == preferred)
                    .cloned()
            })
            .or_else(|| {
                extracted
                    .audio
                    .iter()
                    .find(|info| info.codec != CodecType::TelephoneEvent)
                    .cloned()
            })?;
        Some((chosen.codec, chosen.to_params(), extracted.dtmf.clone()))
    }

    pub fn matching_bridge_selection_from_sdp(
        sdp: &str,
        preferred_codec: Option<CodecType>,
    ) -> Option<BridgeSelection> {
        Self::preferred_audio_from_sdp(sdp, preferred_codec)
            .map(|selection| BridgeSelection::from_audio_tuple(selection, Self::ssrc_from_sdp(sdp)))
    }

    async fn find_track(&self, track_id: &str) -> Option<Arc<AsyncMutex<Box<dyn Track>>>> {
        let tracks = self.peer.get_tracks().await;
        for track in tracks {
            let guard = track.lock().await;
            if guard.id() == track_id {
                drop(guard);
                return Some(track);
            }
        }
        None
    }

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("RTP/SAVPF")
    }

    fn build_track_builder(
        &self,
        track_id: String,
        codec_info: Vec<CodecInfo>,
        media_config: &MediaConfig,
        is_webrtc: bool,
    ) -> RtpTrackBuilder {
        let mut track_builder = RtpTrackBuilder::new(track_id)
            .with_cancel_token(self.peer.cancel_token())
            .with_codec_info(codec_info)
            .with_enable_latching(media_config.enable_latching);

        if let Some(ref servers) = media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref addr) = media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }

        let (start_port, end_port) = if is_webrtc {
            (media_config.webrtc_port_start, media_config.webrtc_port_end)
        } else {
            (media_config.rtp_start_port, media_config.rtp_end_port)
        };

        if let (Some(start), Some(end)) = (start_port, end_port) {
            track_builder = track_builder.with_rtp_range(start, end);
        }
        if is_webrtc {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        track_builder
    }

    pub async fn create_local_track_offer(
        &mut self,
        track_id: String,
        is_webrtc: bool,
        caller_offer_sdp: Option<&str>,
        allow_codecs: &[CodecType],
        media_config: &MediaConfig,
    ) -> Result<String> {
        self.peer.remove_track("ringback-track", true).await;
        let caller_rtp_map = caller_offer_sdp
            .and_then(|caller_offer| {
                SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer).ok()
            })
            .and_then(|sdp| {
                sdp.media_sections
                    .iter()
                    .find(|m| m.kind == rustrtc::MediaKind::Audio)
                    .map(MediaNegotiator::parse_rtp_map_from_section)
            })
            .unwrap_or_default();
        let codec_info =
            Self::build_local_offer_codec_info_from_rtp_map(&caller_rtp_map, allow_codecs);

        let track_builder =
            self.build_track_builder(track_id.clone(), codec_info, media_config, is_webrtc);

        if let Some(existing) = self.find_track(&track_id).await {
            let guard = existing.lock().await;
            let offer = guard.local_description().await?;
            self.offer_sdp = Some(offer.clone());
            Ok(offer)
        } else {
            let track = track_builder.build();
            let offer = track.local_description().await?;
            self.peer.update_track(Box::new(track), None).await;
            self.offer_sdp = Some(offer.clone());
            Ok(offer)
        }
    }

    pub async fn create_caller_answer(
        &mut self,
        track_id: &str,
        codec_info: Vec<CodecInfo>,
        caller_offer_sdp: Option<&str>,
        media_config: &MediaConfig,
    ) -> Result<String> {
        if let Some(answer) = self.answer_sdp.as_ref() {
            return Ok(answer.clone());
        }

        let is_webrtc = caller_offer_sdp
            .map(Self::is_webrtc_sdp)
            .unwrap_or(false);
        let track_builder =
            self.build_track_builder(track_id.to_string(), codec_info, media_config, is_webrtc);

        let answer = if let Some(offer) = caller_offer_sdp {
            if offer.trim().is_empty() {
                let track = track_builder.build();
                let sdp = track.local_description().await?;
                self.peer.update_track(Box::new(track), None).await;
                sdp
            } else if let Some(existing) = self.find_track(track_id).await {
                let guard = existing.lock().await;
                guard.handshake(offer.to_string()).await?
            } else {
                let track = track_builder.build();
                let processed = track.handshake(offer.to_string()).await?;
                self.peer.update_track(Box::new(track), None).await;
                processed
            }
        } else {
            let track = track_builder.build();
            let sdp = track.local_description().await?;
            self.peer.update_track(Box::new(track), None).await;
            sdp
        };

        self.answer_sdp = Some(answer.clone());
        Ok(answer)
    }

    pub async fn apply_remote_answer(
        &mut self,
        track_id: &str,
        remote_sdp: &str,
        media_config: &MediaConfig,
    ) -> Result<()> {
        if let Some(ref existing_sdp) = self.answer_sdp {
            if existing_sdp == remote_sdp {
                return Ok(());
            }
        }

        if let Err(_e) = self.peer.update_remote_description(track_id, remote_sdp).await {
            let mut track = RtpTrackBuilder::new(track_id.to_string())
                .with_cancel_token(self.peer.cancel_token())
                .with_enable_latching(media_config.enable_latching);
            if let Some(ref servers) = media_config.ice_servers {
                track = track.with_ice_servers(servers.clone());
            }
            if let Some(ref addr) = media_config.external_ip {
                track = track.with_external_ip(addr.clone());
            }
            let rtp_track = track.build();
            rtp_track.local_description().await?;
            rtp_track.set_remote_description(remote_sdp).await?;
            self.peer.update_track(Box::new(rtp_track), None).await;
        }

        self.answer_sdp = Some(remote_sdp.to_string());
        self.ssrc = MediaNegotiator::extract_ssrc(remote_sdp);
        Ok(())
    }

    pub async fn update_remote_offer(&self, track_id: &str, offer: &str) -> Result<()> {
        self.peer.update_remote_description(track_id, offer).await
    }

    pub fn select_or_store_negotiated_audio(
        &mut self,
        negotiated_audio: Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    ) {
        self.negotiated_audio = negotiated_audio;
    }

    pub fn freeze_answered_audio<F>(&mut self, selector: F)
    where
        F: Fn(&str) -> Option<(CodecType, rustrtc::RtpCodecParameters, Vec<CodecInfo>)>,
    {
        if self.negotiated_audio.is_some() {
            return;
        }
        self.negotiated_audio = self
            .answer_sdp
            .as_deref()
            .and_then(&selector)
            .or_else(|| self.offer_sdp.as_deref().and_then(selector));
    }

    pub fn freeze_answered_audio_with_allow_codecs(&mut self, allow_codecs: &[CodecType]) {
        self.freeze_answered_audio(|sdp| Self::select_best_audio_from_sdp(sdp, allow_codecs));
    }

    pub async fn remove_ringback_track(&self, ringback_track_id: &str) {
        self.peer.remove_track(ringback_track_id, true).await;
    }

    pub async fn remove_track(&self, track_id: &str) {
        self.peer.remove_track(track_id, true).await;
    }

    pub fn bridge_audio_selection(&self, allow_codecs: &[CodecType]) -> Option<BridgeSelection> {
        self.negotiated_audio
            .clone()
            .map(|selection| BridgeSelection::from_audio_tuple(selection, self.answered_ssrc()))
            .or_else(|| {
                self.answer_sdp
                    .as_deref()
                    .and_then(|sdp| Self::bridge_selection_from_sdp(sdp, allow_codecs))
            })
            .or_else(|| {
                self.offer_sdp
                    .as_deref()
                    .and_then(|sdp| Self::bridge_selection_from_sdp(sdp, allow_codecs))
            })
    }

    pub fn bridge_audio_matching(&self, preferred_codec: Option<CodecType>) -> Option<BridgeSelection> {
        self.answer_sdp
            .as_deref()
            .and_then(|sdp| Self::matching_bridge_selection_from_sdp(sdp, preferred_codec))
            .or_else(|| {
                self.offer_sdp
                    .as_deref()
                    .and_then(|sdp| Self::matching_bridge_selection_from_sdp(sdp, preferred_codec))
            })
    }

    pub fn answered_ssrc(&self) -> Option<u32> {
        self.answer_sdp
            .as_ref()
            .and_then(|sdp| MediaNegotiator::extract_ssrc(sdp))
    }

    pub fn ssrc_from_sdp(sdp: &str) -> Option<u32> {
        MediaNegotiator::extract_ssrc(sdp)
    }

    pub async fn create_file_track(&self, track_id: &str, file_path: &str, loop_playback: bool) {
        let mut track = FileTrack::new(track_id.to_string());
        track = track
            .with_path(file_path.to_string())
            .with_loop(loop_playback);
        self.peer.update_track(Box::new(track), None).await;
    }

    pub async fn create_hold_track(
        &self,
        track_id: &str,
        audio_file: &str,
        loop_playback: bool,
    ) -> Result<()> {
        let caller_codec = self
            .offer_sdp
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().map(|c| c.codec))
            .unwrap_or(CodecType::PCMU);

        let hold_ssrc = rand::random::<u32>();
        let track = FileTrack::new(track_id.to_string())
            .with_path(audio_file.to_string())
            .with_loop(loop_playback)
            .with_ssrc(hold_ssrc)
            .with_codec_preference(vec![caller_codec]);

        let caller_pc = {
            let tracks = self.peer.get_tracks().await;
            if let Some(t) = tracks.first() {
                t.lock().await.get_peer_connection().await
            } else {
                None
            }
        };

        if let Err(e) = track.start_playback_on(caller_pc).await {
            return Err(anyhow!("start_playback failed: {}", e));
        }

        self.peer.update_track(Box::new(track), None).await;
        Ok(())
    }

    pub async fn create_or_switch_hold_track(
        &self,
        track_id: &str,
        audio_file: &str,
        loop_playback: bool,
    ) -> Result<()> {
        if let Some(track_handle) = self.find_track(track_id).await {
            let mut track_guard = track_handle.lock().await;
            if let Some(file_track) = track_guard.as_any_mut().downcast_mut::<FileTrack>() {
                if file_track
                    .switch_audio_source(audio_file.to_string(), loop_playback)
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }

        self.create_hold_track(track_id, audio_file, loop_playback).await
    }

    pub fn set_dtmf_listener_cancel(&mut self, cancel: CancellationToken) {
        self.dtmf_listener_cancel = Some(cancel);
    }

    pub fn cancel_dtmf_listener(&mut self) {
        if let Some(cancel) = self.dtmf_listener_cancel.take() {
            cancel.cancel();
        }
    }

    pub async fn play_prompt_on_peer(
        &self,
        file_path: &str,
        track_id: &str,
        loop_playback: bool,
        cancel_token: CancellationToken,
        app_event_tx: Option<mpsc::UnboundedSender<ControllerEvent>>,
    ) -> Result<()> {
        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !std::path::Path::new(file_path).exists() {
            if let Some(tx) = app_event_tx {
                let _ = tx.send(ControllerEvent::AudioComplete {
                    track_id: track_id.to_string(),
                    interrupted: true,
                });
            }
            return Ok(());
        }

        let caller_tracks = self.peer.get_tracks().await;
        let mut caller_codec_info = None;
        for track in &caller_tracks {
            let guard = track.lock().await;
            if guard.id() == "prompt" {
                continue;
            }
            caller_codec_info = guard.preferred_codec_info();
            if caller_codec_info.is_some() {
                break;
            }
        }
        let caller_codec_info = caller_codec_info.or_else(|| {
            self.offer_sdp
                .as_ref()
                .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
                .and_then(|codecs| codecs.first().cloned())
        });
        let caller_codec = caller_codec_info
            .as_ref()
            .map(|info| info.codec)
            .unwrap_or(CodecType::PCMU);

        let mut track = FileTrack::new("prompt".to_string())
            .with_path(file_path.to_string())
            .with_loop(loop_playback)
            .with_cancel_token(cancel_token.child_token())
            .with_codec_preference(vec![caller_codec]);
        if let Some(info) = caller_codec_info {
            track = track.with_codec_info(info);
        }

        let caller_pc = if let Some(t) = caller_tracks.first() {
            t.lock().await.get_peer_connection().await
        } else {
            None
        };

        if let Err(e) = track.start_playback_on(caller_pc).await {
            if let Some(tx) = app_event_tx {
                let _ = tx.send(ControllerEvent::AudioComplete {
                    track_id: track_id.to_string(),
                    interrupted: true,
                });
            }
            return Err(anyhow!("start_playback failed: {}", e));
        }

        let track_for_wait = track.clone();
        if let Some(event_tx) = app_event_tx {
            let cancel = cancel_token.child_token();
            let tid = track_id.to_string();
            crate::utils::spawn(async move {
                tokio::select! {
                    _ = track_for_wait.wait_for_completion() => {
                        let _ = event_tx.send(ControllerEvent::AudioComplete {
                            track_id: tid,
                            interrupted: false,
                        });
                    }
                    _ = cancel.cancelled() => {
                        let _ = event_tx.send(ControllerEvent::AudioComplete {
                            track_id: tid,
                            interrupted: true,
                        });
                    }
                }
            });
        } else {
            debug!("app_event_tx not set for prompt playback");
        }

        self.peer
            .update_track(Box::new(track), Some("prompt".to_string()))
            .await;
        Ok(())
    }

    pub async fn start_directional_recording(&mut self, option: RecorderOption) -> Result<()> {
        self.directional_recorder_option = Some(option);
        Ok(())
    }

    pub async fn stop_directional_recording(&mut self) -> Result<()> {
        self.directional_recorder_option = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::MediaConfig;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex as AsyncMutex;

    struct TrackingMediaPeer {
        update_count: Arc<AtomicUsize>,
        tracks: Arc<AsyncMutex<Vec<Arc<AsyncMutex<Box<dyn Track>>>>>>,
        cancel_token: CancellationToken,
    }

    impl TrackingMediaPeer {
        fn new() -> Self {
            Self {
                update_count: Arc::new(AtomicUsize::new(0)),
                tracks: Arc::new(AsyncMutex::new(Vec::new())),
                cancel_token: CancellationToken::new(),
            }
        }
    }

    #[async_trait]
    impl MediaPeer for TrackingMediaPeer {
        fn cancel_token(&self) -> CancellationToken {
            self.cancel_token.clone()
        }

        async fn update_track(&self, track: Box<dyn Track>, _play_id: Option<String>) {
            self.update_count.fetch_add(1, Ordering::SeqCst);
            self.tracks
                .lock()
                .await
                .push(Arc::new(AsyncMutex::new(track)));
        }

        async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>> {
            self.tracks.lock().await.clone()
        }

        async fn update_remote_description(&self, _track_id: &str, _sdp: &str) -> Result<()> {
            Ok(())
        }

        async fn renegotiate_track(&self, _track_id: &str, _remote_offer: &str) -> Result<String> {
            Ok("v=0\r\n".to_string())
        }

        async fn suppress_forwarding(&self, _track_id: &str) {}
        async fn resume_forwarding(&self, _track_id: &str) {}
        fn is_suppressed(&self, _track_id: &str) -> bool {
            false
        }
        async fn remove_track(&self, _track_id: &str, _stop: bool) {}
        async fn serve(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) {
            self.cancel_token.cancel();
        }
    }

    #[test]
    fn test_cancel_dtmf_listener_clears_token() {
        let peer = crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new();
        let mut endpoint = MediaEndpoint::new(Arc::new(peer), None);
        let token = CancellationToken::new();
        endpoint.set_dtmf_listener_cancel(token.clone());
        endpoint.cancel_dtmf_listener();
        assert!(token.is_cancelled());
        assert!(endpoint.dtmf_listener_cancel.is_none());
    }

    #[tokio::test]
    async fn test_create_caller_answer_returns_cached_sdp_on_duplicate_calls() {
        let peer = Arc::new(TrackingMediaPeer::new());
        let caller_offer = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";
        let codec_info = MediaNegotiator::extract_codec_params(caller_offer).audio;
        let mut endpoint = MediaEndpoint::new(peer.clone(), Some(caller_offer.to_string()));
        let media_config = MediaConfig::new();

        let answer = endpoint
            .create_caller_answer(
                "caller-track",
                codec_info.clone(),
                Some(caller_offer),
                &media_config,
            )
            .await
            .unwrap();
        let updates_after_first = peer.update_count.load(Ordering::SeqCst);

        let cached = endpoint
            .create_caller_answer("caller-track", codec_info, Some(caller_offer), &media_config)
            .await
            .unwrap();

        assert_eq!(answer, cached);
        assert_eq!(updates_after_first, 1);
        assert_eq!(peer.update_count.load(Ordering::SeqCst), 1);
    }
}
