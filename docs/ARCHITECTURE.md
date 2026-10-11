# Audio and peer boundaries

The peer core accepts original local PCM and returns decoded remote PCM. It owns no device, OS settings, saved file or mixer. The application supplies nondecreasing local time and routes datagrams from its configured peer endpoint.

## Contact, source and permission

Stable member identity, process-run identity, current contact and saved user intent have separate meanings. Fresh contact proof and latest whole-state snapshots reject stale handshakes and reconcile permissions. Each directed stream requires the sender's intent and the receiver's intent. Local refusal is immediate; playback mute and gain are downstream controls.

Version 2 media packets carry a monotonic source epoch, format and source frame index. A source reset allows a new format/frame timeline without reopening healthy sessions or reverse directions. A receiver retains the highest accepted epoch for the same remote run across recontact/regrant and rejects older source packets. The 68-byte media header leaves room for 141 stereo F32 frames within a 1,200-byte datagram. Finite F32 samples retain their bits; non-finite samples are rejected.

The application associates playback with remote run/contact identity as well as source epoch. A replaced remote run or contact requires retiring its queued media outside realtime work. The peer core does not manage that queue or physical deadlines.

## Playback and Windows wiring

DirectPcmPlayout is one prepared, fixed-format source timeline. Source positions determine order; true local arrival and current observation are separate clocks inputs for freshness. Bounded rings reject duplicates/late epochs, discard expired packets and jump forward after backlog or rendering stalls. Direct unity reads preserve finite sample bits; unavailable frames become silence. This primitive has no sample-rate converter or transition fade.

The Windows lab separates original capture, optional original monitoring and decoded returning audio. Only original capture reaches publication. Explicit source/output devices prevent same-endpoint digital recapture. Sources may use render-endpoint loopback or a selected capture endpoint such as a virtual cable recording device. Application routing and source suppression are configured separately in this slice.

Dedicated audio threads retain their COM apartment, device stream and MMCSS registration. Raw callback packets use preallocated pools, independent bounded channels and prepared render scratch. Encoding, networking and coordinator polling stay outside these workers. The render worker submits at most one reported period per event; its configurable buffer policy covers its selected render quantum. There is no hidden limiter, and explicit gain/mix behavior remains distinct from original publication.

Windows diagnostics retain supported shared period ranges, requested periods, initialization failures, buffer capacity and padding. Low-period requests may fall back. Native 48 kHz stereo F32 is the lab's accepted device format. Complete native format/channel support, audio clock matching, headroom policy, device recovery and real audible timing remain subsequent work.

## LAN cooperation

An address/contact helper can introduce peers and expose current presence without carrying media. OpenWrt cooperation is part of the intended deployment; its generic interface remains to be defined. The device-free lab uses configured endpoints and the audio lab uses local UDP only.

Keep configuration and coordination outside audio deadlines. Credentials, household topology, router configuration and private integration records do not belong to public interface examples.
