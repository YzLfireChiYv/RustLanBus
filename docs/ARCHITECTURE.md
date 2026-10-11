# Audio and peer boundaries

The peer foundation accepts original local PCM from its caller and returns decoded remote PCM to its caller. It owns no audio device, operating-system settings, saved file or playback mixer. The application supplies monotonic local time and routes bytes from the configured peer endpoint.

## Contact and permission

Stable member identity and saved user intent are separate from a current run/contact session. A fresh contact proof prevents an old handshake or audio packet from reopening a new session. Complete state snapshots reconcile the latest intent. Each directed stream requires local send intent and the peer's receive intent.

Refusal applies locally immediately. Local playback gain/mute is downstream and does not grant transmission or reception. Reconnect preserves configured intent and the current original-source timeline while refreshing contact state.

## Source and playback

Only captured original local source data belongs on a publication path. Remote decoding and local output mixing must remain separate from that path. An acquisition adapter must exclude application output or provide a distinct route to prevent recapture.

The media foundation preserves finite F32 sample bits, source frame indexes and bounded packet sizes. It rejects non-finite data. It has a bounded duplicate/reordering window, with frame metadata left for the later playback timeline. It supplies no physical playback deadline or sample-rate correction.

Future playback uses the selected output clock with independently qualified input-rate matching before mixing. Expired cache after substantial stalls is discarded and playback returns to current sound. High-quality clock matching, mixer headroom, actual device periods and endpoint queueing require focused device evidence.

## LAN coordination and OpenWrt

An address/contact helper can introduce peers and expose current presence without carrying the audio stream. OpenWrt collaboration is part of the intended deployment. Its precise public interface remains to be defined; the initial lab uses explicit peer addresses.

Keep coordination and configuration work outside audio deadlines. Deployment credentials, household network topology, router configuration and private integration records do not belong to public interface examples.

## Current implementation

The public tree contains the peer core, model/clock/codec/audio support, a Windows adapter and the device-free UDP lab. The Windows adapter has a repaired acquisition-gap accounting contract verified through the common reblocker in pure tests. Live device recovery, source-epoch switching, realtime preallocation and end-to-end delay are subsequent integration work.
