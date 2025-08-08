use crate::{event::SessionEvent, synthesis::SynthesisProgress};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, Stream, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{future::ready, pin::Pin};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing::{debug};
use uuid::Uuid;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api
#[derive(Debug)]
pub struct AliyunTtsClient {
    option: SynthesisOption,
}

/// run-task command structure
#[derive(Debug, Serialize)]
struct RunTaskCommand {
    header: CommandHeader,
    payload: RunTaskPayload,
}

#[derive(Debug, Serialize)]
struct CommandHeader {
    action: String,
    task_id: String,
    streaming: String,
}

#[derive(Debug, Serialize)]
struct RunTaskPayload {
    task_group: String,
    task: String,
    function: String,
    model: String,
    parameters: RunTaskParameters,
    input: EmptyInput,
}

#[derive(Debug, Serialize)]
struct RunTaskParameters {
    text_type: String,
    voice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pitch: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_ssml: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PayloadInput {
    text: String,
}

#[derive(Debug, Serialize)]
struct ContinueTaskCommand {
    header: CommandHeader,
    payload: ContinueTaskPayload,
}

#[derive(Debug, Serialize)]
struct ContinueTaskPayload {
    input: PayloadInput,
}

/// finish-task command structure
#[derive(Debug, Serialize)]
struct FinishTaskCommand {
    header: CommandHeader,
    payload: FinishTaskPayload,
}

#[derive(Debug, Serialize)]
struct FinishTaskPayload {
    input: EmptyInput,
}

#[derive(Debug, Serialize)]
struct EmptyInput {}

/// WebSocket event response structure
#[derive(Debug, Deserialize)]
struct WebSocketEvent {
    header: WebSocketEventHeader,
    payload: WebSocketEventPayload,
}

#[derive(Debug, Deserialize)]
struct WebSocketEventPayload {
    usage: Option<PayloadUsage>,
}

#[derive(Debug, Deserialize)]
struct PayloadUsage {
    characters: u32,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct WebSocketEventHeader {
    task_id: String,
    event: String,
    error_code: Option<String>,
    error_message: Option<String>,
}

struct AliyunSynthesisProgress {
    subtitles: String,
    records: Vec<AliyunSynthesisRecord>,
}
struct AliyunSynthesisRecord {
    audio_length: u32,
    characters: u32,
}

impl SynthesisProgress for AliyunSynthesisProgress {
    fn get_current_progress(&self, current: u32) -> Option<SessionEvent> {
        for (index, record) in self.records.iter().enumerate().rev() {
            if current > record.audio_length {
                continue;
            }

            let byte_index = if index > 0 {
                self.records[index - 1].characters
            } else {
                0
            };

            let position = self
                .subtitles
                .char_indices()
                .position(|(i, _)| byte_index as usize == i)? as u32;

            return Some(SessionEvent::OnInterrupt {
                subtitle: self.subtitles.clone(),
                position,
                total_duration: record.audio_length as u32,
                current,
            });
        }
        None
    }
}

impl AliyunTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self { option }
    }

    /// Get API Key from configuration or environment
    fn get_api_key(&self, option: &SynthesisOption) -> Result<String> {
        option
            .secret_key
            .clone()
            .or_else(|| std::env::var("DASHSCOPE_API_KEY").ok())
            .ok_or_else(|| {
                anyhow!("Aliyun API Key not configured, please set DASHSCOPE_API_KEY environment variable or specify secret_key in configuration")
            })
    }

    /// Create run-task command
    fn create_run_task_command(&self, option: &SynthesisOption, task_id: &str) -> RunTaskCommand {
        let model = option
            .extra
            .as_ref()
            .and_then(|e| e.get("model"))
            .cloned()
            .unwrap_or_else(|| "cosyvoice-v2".to_string());

        let voice = option
            .speaker
            .clone()
            .unwrap_or_else(|| "longyumi_v2".to_string());

        let format = match option.codec.as_deref() {
            Some("mp3") => "mp3",
            Some("wav") => "wav",
            _ => "pcm",
        };

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let volume = (option.volume.unwrap_or(5) * 10) as u32; // Convert to 0 - 100 range
        let rate = option.speed.unwrap_or(1.0);

        RunTaskCommand {
            header: CommandHeader {
                action: "run-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: RunTaskPayload {
                task_group: "audio".to_string(),
                task: "tts".to_string(),
                function: "SpeechSynthesizer".to_string(),
                model,
                parameters: RunTaskParameters {
                    text_type: "PlainText".to_string(),
                    voice,
                    format: Some(format.to_string()),
                    sample_rate: Some(sample_rate),
                    volume: Some(volume),
                    rate: Some(rate),
                    pitch: None,
                    enable_ssml: None,
                },
                input: EmptyInput {},
            },
        }
    }

    fn create_continue_task_command(&self, task_id: &str, text: &str) -> ContinueTaskCommand {
        ContinueTaskCommand {
            header: CommandHeader {
                action: "continue-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: ContinueTaskPayload {
                input: PayloadInput {
                    text: text.to_string(),
                },
            },
        }
    }

    /// Create finish-task command
    fn create_finish_task_command(&self, task_id: &str) -> FinishTaskCommand {
        FinishTaskCommand {
            header: CommandHeader {
                action: "finish-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: FinishTaskPayload {
                input: EmptyInput {},
            },
        }
    }
}

#[async_trait]
impl SynthesisClient for AliyunTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        let option = self.option.merge_with(option);
        let api_key = self.get_api_key(&option)?;
        let task_id = Uuid::new_v4().to_string();
        let ws_url = option
            .endpoint
            .as_deref()
            .unwrap_or("wss://dashscope.aliyuncs.com/api-ws/v1/inference");
        debug!("Connecting to Aliyun WebSocket URL: {}", ws_url);

        let mut request = ws_url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
        headers.insert("X-DashScope-DataInspection", "enable".parse()?);

        // Establish WebSocket connection
        let (ws_stream, response) = connect_async(request).await?;

        // Check connection status
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(anyhow!(
                "WebSocket connection failed: {}",
                response.status()
            ));
        }

        debug!("WebSocket connection established");

        // Split read/write streams
        let (mut ws_sink, ws_stream) = ws_stream.split();

        // Send run-task command
        let run_task_cmd = self.create_run_task_command(&option, &task_id);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        debug!("Sending run-task command: {}", run_task_json);
        ws_sink
            .send(Message::text(run_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send run-task command: {}", e))?;

        // send continue-task command, tts input text send in here
        let continue_task_cmd = self.create_continue_task_command(&task_id, text);
        let continue_task_json = serde_json::to_string(&continue_task_cmd)?;
        debug!("Sending continue-task command: {}", continue_task_json);
        ws_sink
            .send(Message::text(continue_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send continue-task command: {}", e))?;

        // Send finish-task command
        let finish_task_cmd = self.create_finish_task_command(&task_id);
        let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
        debug!("Sending finish-task command: {}", finish_task_json);
        ws_sink
            .send(Message::text(finish_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send finish-task command: {}", e))?;

        let task_id_clone = task_id.clone();
        let stream = ws_stream
            .take_while(move |message| {
                if let Ok(Message::Text(text)) = message
                    && let Ok(event) = serde_json::from_str::<WebSocketEvent>(&text)
                    && event.header.event.as_str() == "task-finished"
                {
                    debug!("Task: {task_id_clone} finished");
                    return ready(false);
                }

                if let Ok(Message::Close(close_frame)) = message {
                    if let Some(close_frame) = close_frame {
                        debug!(
                            "Task: {task_id_clone} closed: {}, {}",
                            close_frame.code, close_frame.reason
                        );
                    }
                    return ready(false);
                }

                ready(true)
            })
            .filter_map(move |message| {
                let task_id = task_id.clone();
                async move {
                    match message {
                        Ok(Message::Binary(data)) => {
                            debug!("task: {task_id} Received audio data: {} bytes", data.len());
                            Some(Ok(data.to_vec()))
                        }
                        Ok(Message::Text(text)) => {
                            if let Ok(event) = serde_json::from_str::<WebSocketEvent>(&text) {
                                match event.header.event.as_str() {
                                    "task-started" => {
                                        debug!("Task: {task_id} started");
                                        None
                                    }
                                    "task-failed" => {
                                        let error_code = event
                                            .header
                                            .error_code
                                            .unwrap_or_else(|| "Unknown error code".to_string());
                                        let error_msg = event
                                            .header
                                            .error_message
                                            .unwrap_or_else(|| "Unknown error message".to_string());
                                        Some(Err(anyhow!(format!(
                                            "Task {task_id} failed: {error_code}, {error_msg}"
                                        ))))
                                    }
                                    "result-generated" => {
                                        if let Some(usage) = event.payload.usage {
                                            debug!("task: {task_id} usage: {}", usage.characters);
                                        }
                                        None
                                    }
                                    _ => {
                                        debug!("Ignoring unknown event: {event:?}");
                                        None
                                    }
                                }
                            } else {
                                Some(Err(anyhow!(format!(
                                    "Task {task_id} failed to deserialize {text}"
                                ))))
                            }
                        }
                        Err(e) => Some(Err(anyhow!("Task {task_id} websocket error: {e}"))),
                        _ => None,
                    }
                }
            });

        Ok(Box::pin(stream))
    }
}
