use std::sync::{Arc, Mutex};

use crate::synthesis::{SynthesisResult, bytes_size_to_duration};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::BoxStream};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing::{debug, warn};
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

struct AliyunSynthesisState {
    task_id: String,
    text: String,
    bytes_size: u32,
    sample_rate: u32,
    usage: u32,
    finished: bool,
}

impl AliyunSynthesisState {
    fn new(task_id: &str, text: &str, sample_rate: u32) -> Self {
        Self {
            task_id: task_id.to_string(),
            text: text.to_string(),
            bytes_size: 0,
            sample_rate,
            usage: 0,
            finished: false,
        }
    }

    fn progress(&self) -> SynthesisResult {
        let duration = bytes_size_to_duration(self.bytes_size, self.sample_rate);
        let position = self
            .text
            .char_indices()
            .position(|(i, _)| i == self.usage as usize)
            .unwrap_or(self.text.len());

        SynthesisResult::Progress {
            subtitles: self.text.clone(),
            position: position as u32,
            current: duration,
            total: duration,
            finished: self.finished,
        }
    }
}

#[async_trait]
impl SynthesisClient for AliyunTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn synthesize(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<Result<SynthesisResult>>> {
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

        let (ws_stream, response) = connect_async(request).await?;

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(anyhow!(
                "WebSocket connection failed: {}",
                response.status()
            ));
        }

        let (mut ws_sink, ws_stream) = ws_stream.split();

        let run_task_cmd = self.create_run_task_command(&option, &task_id);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        debug!("Sending run-task command: {}", run_task_json);
        ws_sink
            .send(Message::text(run_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send run-task command: {}", e))?;

        // text send here
        let continue_task_cmd = self.create_continue_task_command(&task_id, text);
        let continue_task_json = serde_json::to_string(&continue_task_cmd)?;
        debug!("Sending continue-task command: {}", continue_task_json);
        ws_sink
            .send(Message::text(continue_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send continue-task command: {}", e))?;

        // oneshot task
        let finish_task_cmd = self.create_finish_task_command(&task_id);
        let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
        debug!("Sending finish-task command: {}", finish_task_json);
        ws_sink
            .send(Message::text(finish_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send finish-task command: {}", e))?;

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let state = Arc::new(Mutex::new(AliyunSynthesisState::new(
            &task_id,
            text,
            sample_rate,
        )));

        let stream = ws_stream.filter_map(move |message| {
            let state = state.clone();
            async move {
                let mut state = state.lock().unwrap();
                match message {
                    Ok(Message::Binary(data)) => {
                        let bytes_size = data.len() as u32;
                        state.bytes_size += bytes_size;
                        debug!(
                            "Task: {} Received audio data: {} bytes",
                            state.task_id, bytes_size
                        );
                        Some(Ok(SynthesisResult::Audio(data.to_vec())))
                    }
                    Ok(Message::Text(message)) => {
                        match serde_json::from_str::<WebSocketEvent>(&message) {
                            Ok(event) => match event.header.event.as_str() {
                                "task-started" => {
                                    debug!("Task: {} started", state.task_id);
                                    None
                                }
                                "result-generated" => {
                                    if let Some(usage) = event.payload.usage {
                                        let characters = usage.characters;
                                        state.usage = characters;
                                        debug!("Task: {} usage: {}", state.task_id, characters);
                                    }
                                    Some(Ok(state.progress()))
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
                                    Some(Err(anyhow!(
                                        "Task: {} failed: {}, {}",
                                        state.task_id,
                                        error_code,
                                        error_msg
                                    )))
                                }
                                "task-finished" => {
                                    debug!("Task: {} finished", state.task_id);
                                    state.finished = true;
                                    Some(Ok(state.progress()))
                                }
                                _ => {
                                    warn!("Task: {} unknown event: {:?}", state.task_id, event);
                                    None
                                }
                            },
                            Err(e) => {
                                warn!(
                                    "Task: {} failed to deserialize {}: {}",
                                    state.task_id, message, e
                                );
                                None
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        state.finished = true;
                        Some(Ok(state.progress()))
                    }
                    Err(e) => Some(Err(anyhow!(
                        "Task: {} websocket error: {}",
                        state.task_id,
                        e
                    ))),
                    _ => None,
                }
            }
        });

        Ok(Box::pin(stream))
    }
}
