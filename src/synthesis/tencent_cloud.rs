use crate::{event::SessionEvent, synthesis::SynthesisProgress};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{Stream, StreamExt};
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::{future::ready, pin::Pin};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tracing::debug;
use urlencoding;
use uuid;

const HOST: &str = "tts.cloud.tencent.com";
const PATH: &str = "/stream_ws";
/// TencentCloud TTS Response structure
/// https://cloud.tencent.com/document/product/1073/94308   

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct Subtitle {
    pub text: String,
    pub begin_time: u32,
    pub end_time: u32,
    pub begin_index: u32,
    pub end_index: u32,
    pub phoneme: Option<String>,
}

/// WebSocket response structure for real-time TTS
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketResponse {
    pub code: i32,
    pub message: String,
    pub session_id: String,
    pub request_id: String,
    pub message_id: String,
    pub r#final: i32,
    pub result: WebSocketResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSocketResult {
    pub subtitles: Option<Vec<Subtitle>>,
}

#[derive(Debug)]
pub struct TencentCloudTtsClient {
    option: SynthesisOption,
    session_id: String, // session_id for tencent cloud tts request, identity of audio stream
}

struct TencentCloudSynthesisProgress {
    subtitles: String,
    duration: u32,
    records: Vec<Subtitle>,
}

impl SynthesisProgress for TencentCloudSynthesisProgress {
    fn get_current_progress(&self, current: u32) -> Option<SessionEvent> {
        let mut word_count = 0;
        for record in self.records.iter().rev() {
            word_count += record.text.chars().count();
            if current < record.begin_time {
                continue;
            }

            let position = (self.subtitles.chars().count() - word_count) as u32;

            return Some(SessionEvent::OnInterrupt {
                subtitle: self.subtitles.clone(),
                position,
                total_duration: self.duration,
                current,
            });
        }
        None
    }
}

impl TencentCloudTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self {
            option,
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    // Build with specific configuration
    pub fn with_option(mut self, option: SynthesisOption) -> Self {
        self.option = option;
        self
    }

    // Generate WebSocket URL for real-time TTS
    fn generate_websocket_url(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<String> {
        let option = self.option.merge_with(option);
        let secret_id = option.secret_id.clone().unwrap_or_default();
        let secret_key = option.secret_key.clone().unwrap_or_default();
        let app_id = option.app_id.clone().unwrap_or_default();

        let volume = option.volume.unwrap_or(0);
        let speed = option.speed.unwrap_or(0.0);
        let codec = option.codec.clone().unwrap_or_else(|| "pcm".to_string());
        let sample_rate = option.samplerate.unwrap_or(16000);
        let timestamp = chrono::Utc::now().timestamp() as u64;
        let expired = timestamp + 24 * 60 * 60; // 24 hours expiration

        let expired_str = expired.to_string();
        let sample_rate_str = sample_rate.to_string();
        let speed_str = speed.to_string();
        let timestamp_str = timestamp.to_string();
        let volume_str = volume.to_string();
        let voice_type = option
            .speaker
            .clone()
            .unwrap_or_else(|| "601000".to_string());
        let mut query_params = vec![
            ("Action", "TextToStreamAudioWS"),
            ("AppId", app_id.as_str()),
            ("Codec", codec.as_str()),
            ("EnableSubtitle", "true"),
            ("Expired", &expired_str),
            ("SampleRate", &sample_rate_str),
            ("SecretId", secret_id.as_str()),
            ("SessionId", &self.session_id),
            ("Speed", &speed_str),
            ("Text", text),
            ("Timestamp", &timestamp_str),
            ("VoiceType", &voice_type),
            ("Volume", &volume_str),
        ];

        // Sort query parameters by key
        query_params.sort_by(|a, b| a.0.cmp(b.0));

        // Build query string without URL encoding
        let query_string = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let string_to_sign = format!("GET{}{}?{}", HOST, PATH, query_string);

        // Calculate signature using HMAC-SHA1
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret_key.as_bytes());
        let tag = hmac::sign(&key, string_to_sign.as_bytes());
        let signature = STANDARD.encode(tag.as_ref());

        // URL encode parameters for final URL
        let encoded_query_string = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // Build final WebSocket URL
        let url = format!(
            "wss://{}{}?{}&Signature={}",
            HOST,
            PATH,
            encoded_query_string,
            urlencoding::encode(&signature)
        );
        Ok(url)
    }

    // Internal function to synthesize text to audio using WebSocket
    async fn synthesize_text_stream(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        let url = self.generate_websocket_url(text, option)?;
        debug!("connecting to WebSocket URL: {}", url);

        // Create a request with custom headers
        let request = url.into_client_request()?;

        // Connect to WebSocket with custom configuration
        let (ws_stream, resp) = connect_async_with_config(request, None, false).await?;
        match resp.status() {
            reqwest::StatusCode::SWITCHING_PROTOCOLS => (),
            _ => {
                return Err(anyhow::anyhow!(
                    "WebSocket connection failed: {}",
                    resp.status()
                ));
            }
        }

        let session_id = self.session_id.clone();
        let session_id_clone = session_id.clone();
        let stream = ws_stream
            .take_while(move |message| {
                if let Ok(Message::Text(text)) = message
                    && let Ok(response) = serde_json::from_str::<WebSocketResponse>(&text)
                    && response.r#final == 1
                {
                    debug!("TTS Session: {session_id_clone} finished");
                    return ready(false);
                }

                if let Ok(Message::Close(close_frame)) = message {
                    if let Some(close_frame) = close_frame {
                        debug!(
                            "TTS Session: {session_id_clone} closed: {}, {}",
                            close_frame.code, close_frame.reason
                        );
                    }
                    return ready(false);
                }

                ready(true)
            })
            .filter_map(move |message| {
                let session_id = session_id.clone();
                async move {
                    match message {
                        Ok(Message::Binary(data)) => Some(Ok(data.to_vec())),
                        Ok(Message::Text(text)) => {
                            if let Ok(response) = serde_json::from_str::<WebSocketResponse>(&text) {
                                if response.code != 0 {
                                    return Some(Err(anyhow!(format!(
                                        "Tencent TTS faild, Session: {session_id}, error: {}",
                                        response.message
                                    ))));
                                }

                                // TODO: handle text message
                                None
                            } else {
                                Some(Err(anyhow!(format!(
                                    "TTS Session: {session_id} deserialize {text}"
                                ))))
                            }
                        }
                        Err(e) => Some(Err(anyhow!(format!(
                            "Tencent TTS websocket error, Session: {session_id}, error: {e}"
                        )))),
                        _ => None,
                    }
                }
            });
        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }
    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        // Use the new WebSocket streaming implementation
        self.synthesize_text_stream(text, option).await
    }
}
