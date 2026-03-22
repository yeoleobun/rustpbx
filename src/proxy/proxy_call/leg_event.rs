use crate::proxy::proxy_call::state::MidDialogLeg;

/// Reason a leg was terminated.
#[derive(Debug)]
pub(crate) enum TerminationReason {
    ByCaller,
    ByCallee,
}

/// Events emitted by per-leg dialog event tasks.
///
/// Each `CallLeg` spawns its own dialog-event processing task that
/// translates raw `DialogState` transitions into typed `LegEvent`s.
/// The session loop selects over the shared receiver.
#[derive(Debug)]
pub(crate) enum LegEvent {
    /// The dialog on this leg reached the `Terminated` state.
    Terminated { reason: TerminationReason },

    /// A mid-dialog re-INVITE or UPDATE was received.
    ReInvite {
        leg: MidDialogLeg,
        method: rsip::Method,
        sdp: Option<String>,
        dialog_id: String,
    },

    /// A trickle-ICE INFO was received on the exported leg.
    TrickleIce { payload: String },
}
