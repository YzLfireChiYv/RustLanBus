# RustLanBus

An experimental Rust project for precision-preserving, low-delay P2P LAN audio. The goal is to preserve captured audio quality while reducing unnecessary transmission and playback delay.

The current public snapshot contains the first peer/data foundation and its support libraries. It provides real UDP contact, independent send/receive permission, fresh-session recovery and finite F32 PCM datagrams. The Windows acquisition/render library is included for further integration. The lab uses synthetic samples and opens no audio device.

Each direction requires both participants' permission. Only original local sources are intended for publication; received sources and local mixes stay in the local playback path. Source capture, group transmission and playback selection have separate responsibilities.

## Try the peer foundation

Install Rust, then run:

    cargo test -p rlb-peer
    cargo run -p rustlanbus-peer-lab -- demo

The demo uses two local UDP sockets and checks contact, directed refusal/regrant, peer-run recovery and bit-exact finite F32 fixture samples. It does not measure audible latency or demonstrate a finished audio-sharing application.

See [peer-lab usage](apps/peer-lab/README.md) for saved configurations and independent processes. First use needs access to crates.io dependencies. After dependencies are cached, the same commands can use Cargo's --offline option.

## Current boundaries

- One configured peer per controller, composable by a later participant owner.
- One mono/stereo PCM format and forward source timeline per process run.
- PCM packets are bounded to 1,200 bytes. Diagnostic encoding/decoding allocates memory and is outside a realtime callback.
- Device capture/playback integration, source switching, jitter deadlines, high-quality clock correction, discovery and a product UI remain in development.
- Windows device code requires Windows. The portable peer core and diagnostic entry point can be built separately with the package commands above.

Windows is the initial device-integration platform, with macOS and Linux adapters planned. OpenWrt collaboration is part of the intended LAN deployment; coordination remains separate from direct audio. The public [architecture notes](docs/ARCHITECTURE.md) describe only generic interfaces and current implementation boundaries.

## Reference and license

[SonoBus](https://github.com/sonosaurus/sonobus) is a reference for functionality and audio-path design. RustLanBus has its own current peer/media protocol; AOO interoperability has no implementation in this snapshot.

See [LICENSE](LICENSE) and [LICENSE_EXCEPTION](LICENSE_EXCEPTION) for the project's GPLv3 terms and existing additional app-store permission.
