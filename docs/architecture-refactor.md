# Call Architecture: Primitives & Ownership

## Three Primitives

### 1. Leg

One SIP dialog + one MediaPeer. The atom of call handling.

```
Leg {
    id: String,
    direction: Inbound | Outbound,
    sip: SipDialogState,           // dialog IDs, session timer, trickle ICE
    media: MediaState,             // peer, offer/answer SDP, negotiated codec, SSRC
    dtmf: DtmfListener,
    cmd_tx: mpsc::Sender<LegCommand>,
}
```

A Leg can independently:
- Send/receive re-INVITE (codec change, hold/unhold)
- Play audio prompt into its MediaPeer
- Collect DTMF
- Negotiate SDP (offer/answer)
- Manage session timer refreshes

Shared by both proxy-call and RWI — single implementation, replaces the current
split between `CallLeg`+`SipLeg`+`MediaEndpoint` (proxy) and `RwiCallLegRuntime` (RWI).

### 2. Bridge

Connects exactly 2 Legs for bidirectional audio.

```
Bridge {
    id: String,
    leg_a: Arc<Leg>,
    leg_b: Arc<Leg>,
    transcoder: Option<Transcoder>,    // when codecs differ
    recorder: Option<Recorder>,        // mixed or per-leg recording
    sipflow: Option<SipFlowWriter>,    // RTP packet capture
    supervisor_tap: Option<SupervisorMixer>,
}
```

A Bridge can:
- Forward RTP between legs (with transcoding if needed)
- Rewrite DTMF for different clock rates
- Record audio (mixed stereo or separate legs)
- Log RTP to sipflow
- Suppress/resume forwarding per leg (for hold)
- Attach a supervisor tap (listen/whisper/barge)
- Swap one leg for another (transfer)

### 3. Session

The "call" as a business concept. Orchestrates Legs and Bridges.

```
Session {
    id: String,
    context: CallContext,             // immutable: caller/callee identity, dialplan, cookie
    legs: Vec<Arc<Leg>>,              // usually 1-2, but can grow (transfer, conference)
    bridge: Option<Bridge>,           // None during IVR, queue-wait, voicemail
    recording: RecordingState,
    reporter: CallReporter,           // CDR, billing, webhook
    shared: SessionShared,            // observable state for registry + RWI
}
```

A Session decides:
- Which Legs to create and when
- When to bridge/unbridge
- When to start/stop recording
- How to report CDR on hangup

## Feature Composition

Each feature is a combination of these primitives:

```
Feature           Legs      Bridge    Session behavior
───────────────── ───────── ───────── ────────────────────────────────────
Basic call        A + B     A↔B       route → dial B → bridge A↔B → hangup

IVR               A only    none      answer A → play prompts → collect DTMF
                                      app returns Transfer("target") →
                                      create B → bridge A↔B

Queue             A only    none      answer A → play hold music on A
                  (wait)              try dial B₁, B₂, ... sequentially
                  A + Bₙ    A↔Bₙ     first answer → bridge A↔Bₙ

Voicemail         A only    none      answer A → play greeting on A
                                      record A's audio to file → hangup

Transfer          A + B     A↔B       already bridged, then:
                  (+ C)     A↔C       create C → bridge A↔C → drop B

Attended xfer     A + B     A↔B       put A on hold →
                  (+ C)     B↔C       create C, bridge B↔C (consult) →
                            A↔C       bridge A↔C → drop B

Supervisor        A + B     A↔B       listen: tap bridge → send copy to S
 listen/whisper   + S                 whisper: mix S audio into agent's leg only
 /barge                               barge: mix S into both legs

Hold              A + B     A↔B       suppress bridge forwarding for A →
                                      play music into A via A's MediaPeer

Recording         A + B     A↔B       recorder lives on Bridge →
                                      captures both directions
```

## CallApp Interface

The `CallApp` trait operates on a single Leg through `CallController`:

```rust
trait CallApp {
    async fn on_enter(&mut self, ctl: &mut CallController, ctx: &ApplicationContext) -> AppAction;
    async fn on_dtmf(&mut self, ctl: &mut CallController, digit: &str) -> AppAction;
    async fn on_audio_complete(&mut self, ctl: &mut CallController, ...) -> AppAction;
    async fn on_timeout(&mut self, ctl: &mut CallController, name: &str) -> AppAction;
}
```

`CallController` wraps a single `Arc<Leg>` and exposes: answer, play, record, collect_dtmf, hangup.

When the app decides to connect to another party, it returns an `AppAction` (Transfer, Queue, Hangup, etc.) and the **Session** handles creating the outbound Leg and Bridge.

```
Session creates Leg A (caller)
  → answers
  → runs CallApp on Leg A
       │
       ├─ on_enter: play greeting          (Leg A only)
       ├─ on_dtmf("1"): AppAction::Transfer("sip:sales@...")
       │
Session creates Leg B (sales)             ← Session handles this
Session creates Bridge(A, B)
Session starts recording on Bridge
```

## Control Interfaces

Both proxy-call and RWI control Legs through the same interface:

```
                     ┌──────────────┐
                     │  Leg (shared) │
                     │              │
                     │  sip_dialog  │
                     │  media_peer  │
                     │  codec state │
                     │  hold state  │
                     │  dtmf        │
                     └──────┬───────┘
                            │
              Arc<Leg> referenced by both:
              ┌─────────────┴─────────────┐
              │                           │
     CallSession (proxy)          RwiProcessor (API)
     orchestrates via              controls via
     DialplanFlow                  JSON commands

     internally uses:              commands map to:
     - DialplanRuntime             - CallOriginate → create Leg
     - QueueFlow                   - CallBridge → create Bridge
     - AppRuntime (IVR)            - MediaPlay → Leg.play()
     - TargetRuntime               - CallHangup → Leg.hangup()
     - BridgeRuntime               - Hold/Unhold → Leg.hold()
```

## Migration Path from Current Code

| Current | Target |
|---|---|
| `CallLeg` + `SipLeg` + `MediaEndpoint` | Unified `Leg` |
| `RwiCallLegRuntime` + `RwiCallLegState` | Remove, use `Leg` |
| `BridgeRuntime` + `MediaBridge` | Standalone `Bridge` |
| `CallSession` (god object) | Thin `Session` orchestrator |
| `CallSession.recording_state` | `Bridge.recorder` or `Session.recording` |
| `CallSession.supervisor_mixer` | `Bridge.supervisor_tap` |
| `CallSession.negotiation_state` | `Leg.negotiation_state` (per-leg) |
| `CallSession.app_event_tx` | `Session` holds this only while app runs |
| `QueueFlow` / `DialplanRuntime` | Unchanged, operate on `Session` |
| `CallApp` / `CallController` | Unchanged, operate on single `Leg` |

## Key Design Principles

1. **Leg is the atom** — it knows nothing about other legs or business logic.
2. **Bridge connects two atoms** — pure media forwarding + recording.
3. **Session is the molecule** — decides which atoms to create and when to connect them.
4. **One Leg implementation** — no parallel RWI vs proxy leg structs.
5. **Features compose, not inherit** — IVR = Leg + app loop. Queue = Leg + hold + retry dial. No feature-specific fields on Session.
