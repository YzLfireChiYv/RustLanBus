# RustLanBus documentation

English | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

Read these entries in order:

- [Application overview and everyday use](../README.md): desktop, audio setup, CLI, window and tray behavior.
- [Product plan](PLAN.md): target behavior, implementation scope and acceptance boundaries.
- [Architecture](ARCHITECTURE.md): source ownership, PCM/data flow, P2P routes, rules and persistence.
- [Windows first-use guide](FIRST-USE.md): virtual playback endpoints, application routing and local output.
- [Latency boundaries](LATENCY.md): native periods, bounded delivery/conversion and virtual-return timing.

Source implementation and focused compilation establish implementation/build facts. Actual startup, interface, audio and network acceptance are separate. The current framework icon is provisional; no dedicated product icon has an approved design.

English files are the technical originals. Translated editions identify their source file and SHA256. Commands, JSON keys, protocol tags and code examples retain their spelling.
