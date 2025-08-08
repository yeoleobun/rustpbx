use crate::synthesis::SynthesisResult;

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{
    SinkExt, StreamExt,
    future::ready,
    stream::BoxStream,
};
use http::{Request, StatusCode, Uri};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::debug;
/// https://github.com/ruzhila/voiceapi
/// A simple and clean voice transcription/synthesis API with sherpa-onnx
///
#[derive(Debug)]
pub struct VoiceApiTtsClient {
    option: SynthesisOption,
}

/// VoiceAPI TTS Request structure
#[derive(Debug, Serialize, Deserialize, Clone)]
struct TtsRequest {
    text: String,
    sid: i32,
    samplerate: i32,
    speed: f32,
}

/// VoiceAPI TTS metadata response
#[derive(Debug, Serialize, Deserialize)]
struct TtsResult {
    progress: f32,
    elapsed: String,
    duration: String,
    size: i32,
}

impl VoiceApiTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }
    pub fn new(option: SynthesisOption) -> Self {
        Self { option }
    }
    // WebSocket-based TTS synthesis
    async fn ws_synthesize(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<'_, Result<SynthesisResult>>> {
        let option = self.option.merge_with(option);
        let endpoint = option
            .endpoint
            .clone()
            .unwrap_or("ws://localhost:8080".to_string());

        // Convert http endpoint to websocket if needed
        let ws_endpoint = if endpoint.starts_with("http") {
            endpoint
                .replace("http://", "ws://")
                .replace("https://", "wss://")
        } else {
            endpoint
        };
        let chunk_size = 4 * 640;
        let ws_url = format!("{}/tts?chunk_size={}&split=false", ws_endpoint, chunk_size);

        debug!("Connecting to WebSocket URL: {}", ws_url);

        let ws_url = ws_url.parse::<Uri>()?;
        // Create WebSocket request
        let request = Request::builder()
            .uri(&ws_url)
            .header("Host", ws_url.host().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", STANDARD.encode(random::<[u8; 16]>()))
            .body(())?;

        // Connect to WebSocket
        let (ws_stream, response) = connect_async(request).await?;

        // Check if the connection was successful
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(anyhow!(
                "Failed to establish WebSocket connection: {}",
                response.status()
            ));
        }
        debug!("WebSocket connection established");
        // Split WebSocket stream into sender and receiver
        let (mut ws_sender, ws_receiver) = ws_stream.split();
        // Send the TTS request
        ws_sender.send(Message::Text(text.into())).await?;

        let stream = ws_receiver
            .take_while(|message| {
                if let Ok(Message::Text(text)) = message
                    && let Ok(metadata) = serde_json::from_str::<TtsResult>(&text)
                    && metadata.progress >= 1.0 
                {
                    return ready(false);
                }

                if let Ok(Message::Close(_)) = message {
                    return ready(false);
                }

                ready(true)
            })
            .filter_map(|message| async {
                match message {
                    Ok(Message::Binary(data)) => Some(Ok(SynthesisResult::Audio(data.to_vec()))),
                    Ok(Message::Text(text)) => {
                        if let Ok(_) = serde_json::from_str::<TtsResult>(&text) {
                            // TODO: handle progress
                        }
                        None
                    }
                    Err(e) => Some(Err(anyhow!("WebSocket error: {}", e))),
                    _ => None,
                }
            });
        // // Create a stream that will yield audio chunks
        // let stream = Box::pin(stream::unfold(
        //     (ws_receiver, ws_sender, false),
        //     move |(mut read, write, finished)| async move {
        //         // If we've finished processing, end the stream
        //         if finished {
        //             return None;
        //         }

        //         // Receive message from WebSocket
        //         match read.next().await {
        //             Some(Ok(Message::Binary(data))) => {
        //                 let audio_data = data.to_vec();
        //                 Some((Ok(SynthesisResult::Audio(audio_data)), (read, write, false)))
        //             }
        //             Some(Ok(Message::Text(text_data))) => {
        //                 // Text data is metadata
        //                 match serde_json::from_str::<TtsResult>(&text_data) {
        //                     Ok(metadata) => {
        //                         debug!("Received metadata: progress={}, elapsed={}, duration={}, size={}",
        //                               metadata.progress, metadata.elapsed, metadata.duration, metadata.size);

        //                         // If progress is 1.0, this is the final message
        //                         let is_finished = metadata.progress >= 1.0;

        //                         // Return empty chunk and continue or finish
        //                         Some((Ok(SynthesisResult::Audio(Vec::new())), (read, write, is_finished)))
        //                     }
        //                     Err(e) => {
        //                         warn!("Failed to parse metadata: {}", e);
        //                         // Continue receiving data
        //                         Some((Ok(SynthesisResult::Audio(Vec::new())), (read, write, false)))
        //                     }
        //                 }
        //             }
        //             Some(Ok(Message::Close(_))) => {
        //                 // Connection closed
        //                 debug!("WebSocket closed by server");
        //                 None
        //             }
        //             Some(Err(e)) => {
        //                 warn!("WebSocket error: {:?}", e);
        //                 // Error occurred
        //                 Some((Err(anyhow!("WebSocket error: {}", e)), (read, write, true)))
        //             }
        //             _ => {
        //                 // Other message types (ping/pong/etc.)
        //                 Some((Ok(SynthesisResult::Audio(Vec::new())), (read, write, false)))
        //             }
        //         }
        //     },
        // ));

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl SynthesisClient for VoiceApiTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::VoiceApi
    }
    async fn synthesize(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<Result<SynthesisResult>>> {
        self.ws_synthesize(text, option).await
    }
}
