# RustLanBus

An experimental Rust project for precision-preserving, low-delay P2P LAN audio. The goal is to preserve captured audio quality while reducing unnecessary transmission and playback delay.

The current snapshot implements real UDP contact, independent directed send/receive permission, source-epoch changes, fresh-session recovery and finite F32 PCM datagrams. The Windows wiring lab connects explicit native-format devices through local UDP to bounded per-source playback. These are development entry points; the finished participant application is still in development.

Each direction requires both participants' permission. Only original local sources enter publication. Received sources and local mixes stay in the local playback path. Source capture, group transmission and playback selection have separate responsibilities.

## Peer foundation

Install Rust, then run:

    cargo test -p rlb-peer
    cargo run -p rustlanbus-peer-lab -- demo

The device-free demo checks directed contact/refusal/regrant, peer-run recovery and bit-exact finite F32 fixtures using actual local UDP sockets. See [peer-lab usage](apps/peer-lab/README.md) for saved intent and independent processes. Initial use needs crates.io dependencies; cached dependencies permit Cargo's --offline option.

## Windows wiring

    cargo run -p rustlanbus-audio-lab -- devices

The [Windows lab guide](apps/audio-lab/README.md) describes explicit source/output selection, non-playing stream probes and bounded self-loop runs. The initial device path accepts native 48 kHz stereo F32. Original local playback defaults off; local, returning, mixed and muted output can be selected. Buffer policy and explicit source gains are diagnostic choices.

The lab preserves finite PCM on direct unity selection and supplies no hidden limiter. Its current direct-rate playback has no independent-clock correction, recovery fade, hotplug recovery or finished UI. Device processing and physical output have separate quality and latency limits. No end-to-end audible-latency or completed listening-quality result is claimed.

## Current boundaries

- One configured peer per controller, composable by a later participant owner.
- One mono/stereo format and forward timeline per source epoch. Explicit reset preserves healthy peer directions.
- Protocol version 2 uses source epochs and is incompatible with the initial version 1 snapshot; rebuild both participants together.
- Datagrams stay within 1,200 bytes. Diagnostic codecs allocate outside audio workers; prepared audio queues use bounded non-waiting operations.
- General native formats, qualified rate correction, virtual-device provisioning, discovery, automatic device recovery and the real participant UI remain in development.
- Windows device code requires Windows; the portable peer core can be built separately with its package commands.

Windows is the initial device platform, with macOS and Linux adapters planned. Virtual microphone/speaker routing is a fallback direction; community open-source dependencies and guided setup are preferred. OpenWrt cooperation belongs to LAN coordination while audio remains direct. [Architecture notes](docs/ARCHITECTURE.md) describe generic boundaries.

## Reference and license

[SonoBus](https://github.com/sonosaurus/sonobus) is a feature and audio-path reference. RustLanBus currently uses its own peer/media protocol; AOO interoperability has no implementation in this snapshot.

See [LICENSE](LICENSE) and [LICENSE_EXCEPTION](LICENSE_EXCEPTION) for GPLv3 terms and the existing additional app-store permission.
