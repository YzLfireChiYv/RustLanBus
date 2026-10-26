# RustLanBus

> Source status — 2026-10-26: this is a development candidate. The shared audio/control implementation exists; actual startup, interface, audio and network acceptance remain separate. See the [architecture](docs/ARCHITECTURE.md) and [product plan](docs/PLAN.md).

English | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

RustLanBus shares original audio between LAN computers. Its main use is collecting several devices' sound on one Windows computer connected to headphones or speakers. It preserves acquired PCM precision and minimizes avoidable latency, with explicit clock correction, mixing and speaker adaptation for playback.

Windows is the first platform. Desktop and formal CLI share one Rust controller. The 2026-10-26 source adds complete card/settings management, demand-driven acquisition, source/output diagnostics and simultaneous IPv4/IPv6 paths. Product implementation and owner-operated testing are recorded separately. Earlier portable archives remain historical candidates and do not contain these changes. See the [current product plan](docs/PLAN.md) and [architecture](docs/ARCHITECTURE.md).

Public source: [YzLfireChiYv/RustLanBus](https://github.com/YzLfireChiYv/RustLanBus). Public history contains reviewed snapshots; private research and deployment records remain separate. SonoBus is a design/code reference. RustLanBus uses its own protocol and does not interoperate with SonoBus.

## Desktop and audio setup

The desktop uses **Tauri 2, React 19 and TypeScript**, rendered locally by the installed Microsoft Edge WebView2 runtime. Rust owns the shared controller, capture, networking and playback. Audio does not pass through JavaScript.

Extract the portable Windows archive and run `RustLanBus.exe`. Windows requests administrator privileges. The window has a taskbar button; **X** hides it while background audio continues. Click the system-tray icon to restore the window, or choose **Exit** from its menu to stop the program. The window also has an explicit exit action.

Choose a virtual **playback** endpoint as the system loopback source and route applications' sound into it in Windows. Choose physical headphones or speakers as RustLanBus's output. Keeping these endpoints separate keeps received audio out of original capture. Virtual endpoints come from separately installed community drivers; RustLanBus supplies no custom kernel driver. The in-app guide opens official project and Windows settings pages. Follow the [first-use guide](docs/FIRST-USE.md), including the FxSound exit-before-routing order when using that optional virtual device.

Create a room on Windows or join a Windows/OpenWrt room service. The service introduces peer addresses; audio travels directly between members. Passwords prevent accidental entry. Existing direct audio continues if the contact service becomes unavailable. By default one instance binds both IPv4 and IPv6 and probes candidate pairs through the ordinary handshake. Healthy paths remain selected. Advanced settings can strictly restrict the network interface or local address; unavailable restricted networks wait without fallback.

Defaults are **send to all, listen to none; system sound on, microphone off; local monitoring off**. Select members to hear. Listening covers current and later sources, with individual source controls in the expanded details. Sources can share an output, and each source can feed several outputs.

Global and per-room JSON rules explicitly match names, IPs or both. Later matching fields win; room rules run after global rules. Name-only controls affect all same-name devices. Save failures and deciding rules are visible. File edits apply on Reload or next room entry. Discovery does not fill omitted saved IPs. Saved cards have an editor and deletion action; leave the active card before deleting it.

Native integer PCM16/24/32 and F32/F64 retain their declared representation in transport; working buffers use F64. Common mono/stereo and 5.1/7.1 layouts have speaker-aware mappings. Unsupported destinations are reported per source/output. With no receiving demand or local monitoring, native acquisition closes while publication intent, P2P contact and keepalive remain. Current media protocol: RLC5/RLP5; directory protocol: JSON version2 with bounded address alternatives. Earlier development peers must update together.

Optional login startup defaults off. Explicit Windows actions can register an elevated login task or allow this program's inbound TCP/UDP traffic from the local subnet. They do not install audio drivers or change global firewall defaults. Moving the executable can require explicit re-registration; the interface shows the actual registration and offers repair/removal.

On the GL SDK4 OpenWrt target, RustLanBus has an independent native package and Tools page using the official administrator session. Its CGI and short-lived management command work while the contact daemon is stopped. Ordinary application maintenance does not require the legacy gateway controller or alter the proxy/network backend; see [OpenWrt management](services/openwrt-admin/README.md).

## Settings and recovery

Data stays in the Windows user directory. **Application and settings** shows the active configuration and GUI/core build identities, and supports opening another configuration, exporting a single readable JSON settings bundle, previewing an import and choosing its destination. Export includes the currently effective settings, including unsaved values, without changing the original files. It contains room passwords and should be kept with private backups.

Choose **restore this computer** to retain identities and local bindings, or **copy to a new device** to create fresh identities and reconfirm local capture/output devices. Copy mode disables automatic entry, monitoring and login startup. It preserves symbolic Main routes, removes fixed endpoint targets and lists them for reassignment; a fixed-only route stays empty. Preview identifies destination replacement and the retained recovery directory. Existing damaged cards remain available as original bytes in a settings bundle.

Closing the window hides it. Explicit Exit lists any unsaved files and lets you return or deliberately discard them. Importing/switching configuration also identifies pending changes. Human-readable JSON files remain editable outside the program; use Reload to apply those edits.

## Headless Windows entry

CLI and desktop use the same Rust controller. One configuration has one running instance. Run the engine from an elevated terminal; redirected input/output is preserved.

```text
rustlanbus-engine participant --help
rustlanbus-engine participant init local.json --name Headphones --bind 0.0.0.0:0
rustlanbus-engine participant inspect local.json
rustlanbus-engine participant cards local.json
rustlanbus-engine participant run local.json --stdin --json
```

Without `--stdin`, Run stays active until interruption or an explicit `--seconds N` limit. With `--stdin`, EOF stops cleanly. Human commands and command JSON cover rooms, devices, sources, rules, outputs, mix, network selection and settings/card management. `settings pending` lists unsaved files; `settings export PATH` exports live settings. Offline `settings preview` and `settings import` require explicit mode, source revision and replacement confirmation; see command help. Raw internal diagnostic commands are rejected by the formal entry. Use `name "Headphone PC"` to rename while outside a room; the shared controller prevents renaming while joined. JSON mode writes tagged snapshot/result/error records to stdout; help goes to stderr. Offline inspection starts no audio owner.

Example interactive rules listen to a member and exclude its microphone:

```text
rule room --match name Music --listen on
rule room --match name Music --source microphone --listen off
```

Output choices are independent of reception. `main` follows the chosen main output; fixed endpoints wait for recovery when disconnected. Empty output lists keep reception and disable local playback for that scope.

With a return-capable room service, `return start` creates a virtual member returning this computer's originals; `return stop` removes it. Start enables and saves the required ordinary room directions. Timing reports distinguish packet round trip and capture-to-return reception from physical listening delay. The [latency budget](docs/LATENCY.md) explains native periods, bounded queues, delivery reserve and conversion without treating packet RTT as total delay.

## Build and source layout

Actual application, interface, audio, network and test execution is owner-operated. Source inspection and focused build checks support implementation; they establish build status only.

```powershell
cargo check -p rustlanbus-engine -p rustlanbus-ui --offline
cd apps/desktop
npm ci
npm run build
```

`scripts/build-windows.ps1 -Build` assembles a new portable GUI/CLI directory with usage, license and hashes. It does not install drivers, change startup/firewall settings or publish source.

- `crates/rlb-model/`: identities, PCM precision and speaker positions.
- `crates/rlb-peer/`: peer protocol and publication contracts.
- `apps/engine/`: shared participant controller and Windows audio runtime.
- `apps/desktop/`: Tauri shell and React interface.
- `services/contactd/`: room contact and optional virtual return support.
- `crates/rlb-return/`: ordinary virtual return participants.
- `apps/peer-lab/` and `apps/audio-lab/`: explicit development diagnostics.

Licensed under [GNU GPL v3](LICENSE), with the additional permission in [LICENSE_EXCEPTION](LICENSE_EXCEPTION).
