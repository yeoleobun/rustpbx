use crate::media::mixer::MediaMixer;
use crate::media::mixer::{MixerPeer, SupervisorMixerMode};
use crate::media::recorder::RecorderOption;
use crate::proxy::proxy_call::media_bridge::MediaBridge;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use crate::sipflow::SipFlowBackend;
use anyhow::Result;
use audio_codec::CodecType;
use std::sync::Arc;

pub(crate) struct BridgeRuntime {
    pub media_bridge: Option<MediaBridge>,
    pub recorder_option: Option<RecorderOption>,
    pub supervisor_mixer: Option<Arc<MediaMixer>>,
}

impl BridgeRuntime {
    pub fn new(recorder_option: Option<RecorderOption>) -> Self {
        Self {
            media_bridge: None,
            recorder_option,
            supervisor_mixer: None,
        }
    }

    pub fn ensure_media_bridge(
        &mut self,
        leg_a: Arc<dyn MediaPeer>,
        leg_b: Arc<dyn MediaPeer>,
        params_a: rustrtc::RtpCodecParameters,
        params_b: rustrtc::RtpCodecParameters,
        dtmf_codecs_a: Vec<crate::media::negotiate::CodecInfo>,
        dtmf_codecs_b: Vec<crate::media::negotiate::CodecInfo>,
        codec_a: CodecType,
        codec_b: CodecType,
        ssrc_a: Option<u32>,
        ssrc_b: Option<u32>,
        call_id: String,
        sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
    ) {
        if self.media_bridge.is_none() {
            self.media_bridge = Some(MediaBridge::new(
                leg_a,
                leg_b,
                params_a,
                params_b,
                dtmf_codecs_a,
                dtmf_codecs_b,
                codec_a,
                codec_b,
                ssrc_a,
                ssrc_b,
                self.recorder_option.clone(),
                call_id,
                sipflow_backend,
            ));
        }
    }

    pub async fn start_bridge(&self) -> Result<()> {
        if let Some(ref bridge) = self.media_bridge {
            bridge.start().await?;
        }
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.media_bridge.is_some()
    }

    pub fn stop_bridge(&self) {
        if let Some(ref bridge) = self.media_bridge {
            bridge.stop();
        }
    }

    pub fn clear_bridge(&mut self) {
        self.stop_bridge();
        self.media_bridge = None;
    }

    pub async fn resume_forwarding(&self, track_id: &str) -> Result<()> {
        if let Some(ref bridge) = self.media_bridge {
            bridge.resume_forwarding(track_id).await?;
        }
        Ok(())
    }

    pub async fn suppress_or_pause_target_forwarding(
        &self,
        track_id: &str,
        exported_peer: Arc<dyn MediaPeer>,
    ) -> Result<()> {
        if let Some(ref bridge) = self.media_bridge {
            bridge.suppress_forwarding(track_id).await?;
        } else {
            exported_peer.suppress_forwarding(track_id).await;
        }
        Ok(())
    }

    pub async fn resume_or_unpause_target_forwarding(
        &self,
        track_id: &str,
        exported_peer: Arc<dyn MediaPeer>,
    ) -> Result<()> {
        if let Some(ref bridge) = self.media_bridge {
            bridge.resume_forwarding(track_id).await?;
        } else {
            exported_peer.resume_forwarding(track_id).await;
        }
        Ok(())
    }

    pub fn start_supervisor_mode(
        &mut self,
        session_id: &str,
        target_session_id: &str,
        peer: Arc<dyn MediaPeer>,
        mode: SupervisorMixerMode,
    ) {
        let mixer = Arc::new(MediaMixer::new(
            format!("supervisor-{}-{}", session_id, target_session_id),
            8000,
        ));
        mixer.add_input(MixerPeer::new(
            peer,
            "supervisor".to_string(),
            "supervisor-out".to_string(),
        ));
        mixer.set_mode(mode);
        mixer.start();
        self.supervisor_mixer = Some(mixer);
    }

    pub fn stop_supervisor_mode(&mut self) {
        if let Some(mixer) = self.supervisor_mixer.take() {
            mixer.stop();
        }
    }
}
