# Current Media Layer Design

This document describes the current media-layer design in this repository after the proxy media refactor on branch `unified-session`.

It focuses on:

- the anchored proxy-call media path
- the current source-centric bridge model
- playback on the same sender-polled output model
- the remaining legacy media pieces that are still outside this refactor

## Overview

The media layer currently has three shapes:

1. Anchored proxy-call bridge
   - Implemented in [`src/media/link.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/link.rs)
   - This is the current design direction
   - It is sender-polled and does not spawn a forwarding task

2. WebRTC <-> RTP transport bridge
   - Still implemented by the older task-based bridge in [`src/media/bridge.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/bridge.rs)
   - This path is not migrated yet

3. Legacy track/media-stream layer
   - Still present for compatibility in [`src/media/mod.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/mod.rs)
   - No longer the design center for anchored proxy bridging or playback

So the system is still hybrid:

- anchored bridge and playback use the new link model
- transport bridge remains legacy

## Main Design Goal

The current design goal is:

- keep the sender-poll model
- keep media packet-oriented
- remove the old stage pipeline
- make source switching explicit
- keep outbound RTP continuity stable across source switching

That is now implemented for the anchored proxy-call path and file playback.

## Core Concepts

### `MediaSource`

Defined in [`src/media/link.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/link.rs).

`MediaSource` is the only media production abstraction:

- `recv(&mut self) -> MediaResult<MediaSample>`
- packet-oriented only
- mutable and owned by one `DirectedLink`

Current implementations:

- `PeerSource`
  - per-direction bridge source
  - owns filtering, DTMF mapping, transcoding, and receiver-side recording for one bridge direction
- `FileSource`
  - packet-producing playback source
  - uses `AudioSourceManager` internally and emits encoded audio for the target leg
- `IdleSource`
  - detached source
  - `recv()` just waits forever

### `DirectedLink`

`DirectedLink` is now intentionally minimal:

- owns exactly one `MediaSource`
- delegates `recv()` directly to that source

There is no longer a stage pipeline inside the link.

Detached output uses:

- `DirectedLink::idle()`

which is just a link around `IdleSource`.

### `SenderTrackAdapter`

`SenderTrackAdapter` is the sink-facing boundary object that implements `rustrtc::media::MediaStreamTrack`.

It now owns:

- the currently active `DirectedLink`
- a replacement channel for link switching
- one persistent outbound RTP continuity state

Its job is:

- let `RtpSender` poll media
- switch to a new source when commanded
- keep outbound RTP sequence/timestamp continuity stable even when the source changes

Important design choice:

- RTP continuity is adapter-owned, not source-owned
- so switching from peer media to file playback or back does not reset the outbound RTP timeline

### `LegOutput`

`LegOutput` is the stable sender attachment point for one peer leg.

It is responsible for:

- binding one `SenderTrackAdapter` into the target peer connection sender
- replacing the active link
- removing the active link by switching back to the idle link
- switching playback onto the output with `FileSource`

This is the runtime switching boundary for the new design.

### `BidirectionalBridge`

`BidirectionalBridge` is a thin wrapper over two directional source configs:

- caller source -> callee output
- callee source -> caller output

It stores:

- the two source receiver tracks
- the two per-direction `PeerSourceConfig`s
- the two outputs

That lets it do both:

- `bridge()`
  - build and install a `PeerSource` for each direction
- `unbridge()`
  - remove both outputs back to the idle link

## Current Anchored Proxy Bridge

The anchored proxy bridge is wired in [`src/proxy/proxy_call/sip_session.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/proxy/proxy_call/sip_session.rs).

The current flow is:

1. Caller-facing answer SDP and callee answer SDP are available.
2. `MediaNegotiator::extract_leg_profile(...)` builds the selected per-leg media profiles.
3. `SipSession::build_peer_source_config(...)` converts those profiles into one per-direction `PeerSourceConfig`.
4. `SipSession::install_bidirectional_bridge(...)`:
   - gets the source receiver tracks from both peer connections
   - ensures both target outputs exist
   - installs one `BidirectionalBridge`
5. `bridge.bridge().await` activates both directions.

This path is only used for:

- anchored media
- same transport type

The WebRTC <-> RTP task-based bridge is still separate.

## `PeerSource` Behavior

`PeerSource` is now the bridge-direction media adapter.

For one source leg and one target leg, it is built with:

- selected source audio PT / codec / clock rate
- selected target audio PT / codec / clock rate
- selected source DTMF PT / clock rate
- selected target DTMF PT / clock rate or drop behavior
- optional `Transcoder`
- optional recorder binding

`PeerSource::recv()` does:

1. pull the next packet from the source receiver track
2. record the original incoming sample if recording is active
3. if it is the selected DTMF PT:
   - remap PT
   - rescale RFC4733 duration if clock rate changes
   - drop if target leg has no negotiated DTMF
4. if it is the selected audio PT:
   - transcode if codecs differ
   - otherwise rewrite payload type / clock rate to target values
5. drop all other payload types by looping for the next source packet

So:

- DTMF mapping
- audio filtering
- transcoding
- source-side recording

all now live in `PeerSource`, not in a separate stage chain.

## Playback

Playback now uses the same output model as bridging.

`FileSource`:

- uses `AudioSourceManager`
- self-paces with `tokio::time::Interval`
- encodes directly to the selected target codec/PT
- emits completion through an internal `Notify`
- stays pending after natural completion

Session-level playback logic in [`src/proxy/proxy_call/sip_session.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/proxy/proxy_call/sip_session.rs) does:

- select the target output
- build `FileSource`
- install it with `replace_with_file_source(...)`
- emit `audio_complete`
  - `interrupted: false` on natural EOF
  - `interrupted: true` when playback is replaced/stopped/cleaned up by the session

There is no playback push task anymore.

## RTP Continuity

Outbound RTP continuity is now adapter-owned.

`SenderTrackAdapter` keeps one continuity state across link replacement:

- next output RTP timestamp
- next output sequence number
- last observed input timestamp / sequence / clock rate

For each outgoing audio sample, the adapter:

- rewrites sequence number
- rewrites RTP timestamp onto a continuous outbound timeline

That means:

- source switching does not reset the outbound sender timeline
- source-specific mapping/transcoding happens before adapter rewrite

This is why RTP rewrite no longer belongs in `PeerSource` or in a stage object.

## Recording

Recording is now source-side in the anchored bridge path.

Current behavior:

- `PeerSource` records the original incoming packet before remap/transcode
- recorder decoding and file writing still use the existing recorder implementation in [`src/media/recorder.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/recorder.rs)

So recording now captures:

- pre-transcode
- pre-output-remap
- original incoming media for that leg

This is the main recording change from the older output-side pipeline design.

## Concurrency Model

The current anchored bridge path does not spawn a forwarding worker.

The current assumptions are:

- `rustrtc::RtpSender` polls one track serially in its sender loop
- we optimize for that concrete behavior

Current locking:

- `current_link` uses `tokio::sync::Mutex`
- `change_rx` uses `tokio::sync::Mutex`
- replacement happens by sending a full new `DirectedLink` through a bounded channel

`SenderTrackAdapter.recv()`:

- holds the current link and replacement receiver
- `select!`s between:
  - current link producing a sample
  - a new replacement link arriving

If a replacement arrives first:

- the old link is discarded
- the adapter switches immediately to the new one

So source switching can preempt an in-flight source poll.

## Negotiation Assumptions

The current anchored bridge path still uses the existing negotiation assumptions from [`src/media/negotiate.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/negotiate.rs):

- the first answered audio codec is treated as the negotiated audio codec
- one DTMF entry is selected per leg
- if multiple DTMF entries exist:
  - choose `48000` for Opus legs
  - otherwise choose `8000`

These assumptions are still deliberate and not yet generalized.

## What Is Still Legacy

The following are still outside the new source-centric link design:

- [`src/media/bridge.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/bridge.rs)
  - task-based WebRTC <-> RTP bridge
- legacy `Track` / `MediaStream` compatibility in [`src/media/mod.rs`](/Users/yangli/Desktop/miuda/rustpbx/src/media/mod.rs)
- broader `MediaPeer` redesign

So the new design currently applies to:

- anchored proxy bridge
- playback on session outputs

not to every media path in the system yet.
