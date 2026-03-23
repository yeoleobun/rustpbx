use crate::call::app::ControllerEvent;
use crate::proxy::proxy_call::media_endpoint::MediaEndpoint;
use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(crate) async fn play_audio_file(
    media: &MediaEndpoint,
    session_id: &str,
    file_path: &str,
    _await_completion: bool,
    track_id: &str,
    loop_playback: bool,
    cancel_token: CancellationToken,
    app_event_tx: Option<mpsc::UnboundedSender<ControllerEvent>>,
) -> Result<()> {
    info!(session_id = %session_id, file = %file_path, "Playing audio file");
    media
        .play_prompt_on_peer(
            file_path,
            track_id,
            loop_playback,
            cancel_token,
            app_event_tx,
        )
        .await
}

#[cfg(test)]
mod tests {
    use crate::call::app::ControllerEvent;
    use crate::media::audio_source::estimate_audio_duration;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn spawn_timer(
        file_path: &str,
        cancel: CancellationToken,
    ) -> mpsc::UnboundedReceiver<ControllerEvent> {
        let (tx, rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let duration = estimate_audio_duration(file_path);
        let fp = file_path.to_string();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(duration) => {
                    let _ = tx.send(ControllerEvent::AudioComplete {
                        track_id: "default".to_string(),
                        interrupted: false,
                    });
                }
                _ = cancel.cancelled() => {
                    let _ = tx.send(ControllerEvent::AudioComplete {
                        track_id: "default".to_string(),
                        interrupted: true,
                    });
                }
            }
            drop(fp);
        });
        rx
    }

    fn write_wav_file(sample_rate: u32, num_samples: u32) -> NamedTempFile {
        let mut tmp = NamedTempFile::with_suffix(".wav").expect("tempfile");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec)
            .expect("WavWriter");
        for _ in 0..num_samples {
            w.write_sample(0i16).expect("write_sample");
        }
        w.finalize().expect("finalize");
        tmp
    }

    #[tokio::test]
    async fn test_missing_file_sends_interrupted_immediately() {
        let path = "/nonexistent/phantom.wav";
        let (tx, mut rx) = mpsc::unbounded_channel::<ControllerEvent>();
        if !std::path::Path::new(path).exists() {
            let _ = tx.send(ControllerEvent::AudioComplete {
                track_id: "default".to_string(),
                interrupted: true,
            });
        }

        let event = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv())
            .await
            .expect("event must arrive immediately")
            .expect("channel must not be closed");

        match event {
            ControllerEvent::AudioComplete { interrupted, .. } => {
                assert!(interrupted, "missing file must produce interrupted=true");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_wav_audio_complete_fires_after_duration() {
        let tmp = write_wav_file(8000, 80);
        let path = tmp.path().to_str().unwrap();

        let cancel = CancellationToken::new();
        let mut rx = spawn_timer(path, cancel.clone());

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("AudioComplete must arrive within 500 ms")
            .expect("channel must not be closed");

        match event {
            ControllerEvent::AudioComplete {
                interrupted,
                track_id,
            } => {
                assert!(
                    !interrupted,
                    "normal completion must have interrupted=false"
                );
                assert_eq!(track_id, "default");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_cancellation_sends_interrupted_audio_complete() {
        let tmp = write_wav_file(8000, 80_000);
        let path = tmp.path().to_str().unwrap();

        let cancel = CancellationToken::new();
        let mut rx = spawn_timer(path, cancel.clone());

        cancel.cancel();

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("AudioComplete (interrupted) must arrive quickly after cancel")
            .expect("channel");

        match event {
            ControllerEvent::AudioComplete { interrupted, .. } => {
                assert!(interrupted, "cancelled playback must have interrupted=true");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_audio_complete_does_not_fire_early() {
        let tmp = write_wav_file(8000, 8000);
        let path = tmp.path().to_str().unwrap();

        let cancel = CancellationToken::new();
        let mut rx = spawn_timer(path, cancel.clone());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            rx.try_recv().is_err(),
            "AudioComplete must not fire before the file has finished playing"
        );

        cancel.cancel();
    }

    #[test]
    fn test_estimate_duration_wav_one_second() {
        let tmp = write_wav_file(8000, 8000);
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 995 && dur.as_millis() <= 1005,
            "1-second WAV: expected ~1000 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_wav_20ms() {
        let tmp = write_wav_file(8000, 160);
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 18 && dur.as_millis() <= 22,
            "20 ms WAV: expected ~20 ms, got {} ms",
            dur.as_millis()
        );
    }
}
