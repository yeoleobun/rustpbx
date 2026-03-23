use crate::call::app::{ControllerEvent, RecordingInfo};
use crate::media::recorder::RecorderOption;
use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(crate) type RecordingState = Option<(String, Instant)>;

pub(crate) async fn start_recording(
    media: &mut MediaEndpoint,
    session_id: &str,
    recording_state: &mut RecordingState,
    path: &str,
    _max_duration: Option<Duration>,
    beep: bool,
) -> Result<()> {
    info!(session_id = %session_id, path = %path, "Starting recording");

    if beep {
        // Beep handling should probably be separate or handled by caller.
    }

    *recording_state = Some((path.to_string(), Instant::now()));
    media
        .start_directional_recording(RecorderOption::new(path.to_string()))
        .await?;

    Ok(())
}

pub(crate) async fn stop_recording(
    media: &mut MediaEndpoint,
    session_id: &str,
    recording_state: &mut RecordingState,
    app_event_tx: Option<mpsc::UnboundedSender<ControllerEvent>>,
) -> Result<()> {
    info!(session_id = %session_id, "Stopping recording");

    media.stop_directional_recording().await?;

    if let Some((path, start_time)) = recording_state.take() {
        let duration = start_time.elapsed();
        let size_bytes = 0u64;

        if let Some(tx) = app_event_tx {
            let info = RecordingInfo {
                path,
                duration,
                size_bytes,
            };
            let _ = tx.send(ControllerEvent::RecordingComplete(info));
        } else {
            warn!(
                session_id = %session_id,
                "app_event_tx not set — RecordingComplete will not be delivered"
            );
        }
    } else {
        warn!(session_id = %session_id, "No active recording to stop");
    }

    Ok(())
}
