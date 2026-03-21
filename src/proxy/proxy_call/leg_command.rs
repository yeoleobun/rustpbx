use crate::callrecord::CallRecordHangupReason;
use crate::media::recorder::RecorderOption;

/// Commands that can be sent to a `CallLeg`.
///
/// These represent the typed operations that a session or controller
/// can request on a specific leg, replacing direct field access into
/// `leg.sip` / `leg.media`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum LegCommand {
    Hangup {
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    SendBye {
        reason: Option<String>,
    },
    SendCancel,
    PlayAudio {
        file: String,
        track_id: String,
        loop_playback: bool,
    },
    StopPlayback,
    Hold {
        music_source: Option<String>,
    },
    Unhold,
    StartRecording {
        option: RecorderOption,
    },
    StopRecording,
    Transfer {
        target: String,
    },
}
