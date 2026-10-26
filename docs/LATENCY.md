# Audio delay and lifetime boundaries

The objective is preserving acquired PCM precision while minimizing avoidable delay. There is no fixed latency target. Useful whole-path terms are capture-device/Windows acquisition cadence, capture handoff and packetization, one-way network delivery, receiver reserve plus conversion, and native output padding plus device latency. Terms can overlap; their independent maxima cannot be summed into an observed result.

## Native acquisition and output

System sound uses the selected render endpoint's shared loopback path. WASAPI's `InitializeSharedAudioStream` does not accept the loopback flag, so that route uses classic shared initialization. Microphone/output paths attempt the supported shared low-period facilities with recorded fallbacks. Actual period, buffer and native format describe the opened endpoint; a requested minimum is not an achieved period.

No receiver and no local monitor closes native acquisition. Peer contact, grants and keepalive stay active. A new receiver can wake capture through source-role demand before a new native epoch exists. That epoch is acknowledged before PCM is sent. This adds device-start time to first sound while retaining the existing peer connection.

## Handoff, delivery and playback

Capture hands off acquired frames without waiting to fill a larger artificial block. The frame limit bounds a block; it is not a packetization timer. A capture block older than its native-period-based budget is discarded. Packet size remains bounded and the receiver never accelerates obsolete sound to catch up.

Each output owns one clock and renderer. Each source/output timeline has its own converter and delivery reserve. The native endpoint owner starts the generic renderer with no fixed guard, then derives a guard from actual source block cadence, the output period converted to source frames and recent delivery gaps. Two short observation windows allow a past jitter peak to expire. This reserve is bounded by the prepared window and can re-prime the current timeline when changed. The generic `TimingPolicy::provisional()` constant is not the production endpoint's fixed added latency.

Conversion needs input lookahead and has the prepared converter's output delay. Native output padding is a further boundary after software submission. The renderer discards expired backlog and re-primes current sound after an outage. Gain/mute/recovery use a short 2ms amplitude ramp in already-due output frames; it creates no additional audio queue. Revoked directions close immediately. Detached full source windows/converters are released at the cold routing boundary; ingress identity/epoch checks reject obsolete data.

## Virtual-return observations

Packet round trip measures the original packet's local send to corresponding returned reception through the router. Capture-to-return reception also includes local acquisition/handoff ages known to the process. Neither observation contains the final native output, headphone DAC or acoustic delay. A return virtual peer performs no capture, playback, resampling or mixing. Ordinary audio still travels directly between peers; the router return is an explicit optional diagnostic route.

Source/route counters and output phases identify software progress and errors. They do not prove audible output. Real usage and latency tests are operated by the owner; no additional physical verification gate is implied by this document.
