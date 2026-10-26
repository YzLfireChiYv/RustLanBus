# RustLanBus architecture

This document describes the source implementation as of 2026-10-26. Source
coverage and focused compilation establish implementation/build facts. They do
not establish observed stability, audible output, physical latency or complete
product acceptance. Existing portable archives may predate the source. The
framework icon remains provisional; no dedicated application icon is approved.

## Purpose and boundaries

RustLanBus collects original audio from LAN computers onto one or more receiving
computers. Windows is the first platform. Acquired PCM precision is retained in
transport; playback explicitly handles independent device clocks, mixing and
speaker adaptation. Avoidable delay is minimized without a fixed millisecond
target. SonoBus informs the design and code study; interoperability is outside
scope. Internet/NAT operation, recording, hotkeys and custom kernel drivers are
outside the agreed Windows scope.

A room contact directory introduces members and reachable addresses. A Windows
participant can host it; a standalone service can run on another LAN machine or
an OpenWrt device. The directory does not govern a host permission matrix or
relay ordinary peer audio. Established direct paths can continue while directory
recovery runs in the background. The optional virtual-return service returns one
requesting computer's PCM through a separate ordinary virtual member.

## Ownership and control

The Tauri 2 desktop shell and React/TypeScript view use the same Rust participant
controller as the formal headless CLI. One configuration has one running
participant instance; the CLI does not remotely control an existing desktop.
Audio buffers remain in Rust. Commands mutate the authoritative owner; snapshots
expose observed state and bounded diagnostics to the interface.

Room/session orchestration owns joining, contact updates, discovery and saved
room intent. Global preferences remain available while networking is unavailable.
Rules project user intent into explicit source permissions and output plans.
Network connection, native acquisition and output destinations have independent
lifetimes. A quiet peer keeps its control connection while demand-free native
acquisition closes; an existing subscription can request acquisition again.

## Identity, streams and delivery

A member identifies a participant. Each participant can publish independent
system and microphone source roles. A source's run/epoch/format and the peer's
negotiated session bind media to its current lifetime. Packet/control checks
reject obsolete identities before playback admission.

Default intent allows sending and receives from nobody. Listening to a member
includes present and later sources, with source-specific exclusions. Both ends
must allow a direction before media flows. Ordinary participants publish local
originals. Same-name rule scopes deliberately affect all matching members unless
an explicit IP condition narrows the scope.

Current media tags are RLC5/RLP5. Directory JSON is version 2, with bounded
alternate addresses sharing a media port. One media owner manages bounded sockets
on actual local IPv4/IPv6 addresses. The ordinary handshake probes bounded
candidate pairs while disconnected, then pins a healthy local/remote pair.
Session-specific queued control replies preserve that route. A selected path's
identity is confirmed before its IP-derived media policy is applied. Explicit
interface/address restrictions wait when unavailable and never silently fall back.

IP conditions match only the actual selected connection address. A matching advertised IPv4 address does not grant permission to a connection using that member's IPv6 address. The interface exposes the actual address and the deciding rule source.

DNS-SD uses `_rlbroom._tcp.local.` with version/name/instance metadata and an
optional address hint. Discovery and TCP directory availability recover
independently. Discovered addresses never fill an omitted saved-card address.

## Capture, precision and playback

Windows system sound uses shared-mode loopback of the selected playback endpoint.
A separately selected physical output carries local monitoring and remote sound.
An installed community virtual playback device is the supported routing aid;
RustLanBus does not ship a custom driver. Native microphone capture is a separate
source. Source selection and local monitoring stay independent of room membership.

PCM16/24/32 integers and F32/F64 share an explicit precision/speaker descriptor.
Network transport retains the declared representation. Internal working samples
and output accumulation use F64. Each resolved output endpoint has one owner,
one clock and one renderer; several sources can share it, and a source can feed
several outputs. Symbolic Main follows the selected main endpoint. Fixed choices
wait for their device to return. Aliases of one endpoint render once.

Each active source/output lane owns bounded timing storage and conversion state.
Production reserve derives from measured source cadence, output period and recent
delivery variation. Expired audio is discarded; recovery resumes current sound.
Short gain/mute/recovery ramps operate in already-due frames and add no PCM queue.
Detached full DSP lanes retire at the cold control boundary. Ingress bindings and
epoch/session checks carry the minimum stale-data rejection authority.

Common mono/stereo and 5.1/7.1 speaker mappings use the approved Microsoft
coefficients. Unsupported destination combinations report per-source/output
issues while original transport and other outputs continue. The optional limiter
defaults off. Source/output phases, frame counters, errors and levels describe
software progress; they do not prove audible hardware output.

## Persistence and platform integration

Configuration and room cards live in the user's data directory. Editable global
and room JSON rules apply in order: globals first, room rules second, later
matching assignments override specified fields. Name, IP and combined conditions
are explicit. Rule origins and per-file pending/write-error states are projected
to both entries. External edits apply on Reload or the next clean room entry.
Unsaved globals survive room switches; unsaved room changes require a visible
warning before leaving. Explicit Exit identifies unsaved files.

Readable settings bundles export effective values. Restore retains local
identities; copy-to-new-device creates fresh identities and requires local device
confirmation. Import previews replacement, checks revisions and retains recovery
material. Exported bundles can contain room passwords and belong in private
backups.

Windows executables request administrator privileges. The ordinary window has a
taskbar button; closing hides it, the tray restores it, and explicit Exit stops
owners. Optional login startup defaults off. Firewall/startup changes require an
explicit application action and manage only the application's entries. Audio
routing and driver installation remain explicit user operations.

## Source map and builds

- `crates/rlb-model`: identities, PCM precision, formats and speaker positions.
- `crates/rlb-peer`, `rlb-media`, `rlb-contact`, `rlb-return`: direct peer/media
  contracts, room contact and ordinary virtual-return participants.
- `crates/rlb-clock`, `rlb-codec`, `rlb-jitter`, `rlb-net`, `rlb-audio`,
  `rlb-audio-windows`: timing, conversion, bounded delivery and native I/O support.
- `apps/engine/src/participant`: shared controller, rooms/rules, networking,
  capture/playback, settings management and formal CLI.
- `apps/desktop`: Tauri shell, React view, static assets and Windows manifests.
- `services/contactd`: standalone directory, common DNS-SD and optional return.
- Remaining declared workspace crates, registry service and lab/echo tools retain
  model/development roles. Their presence does not make them a second production
  audio path.

The complete workspace and checked-in Cargo/npm lockfiles are included in the
reviewed source snapshot. Desktop builds require the frontend dependencies and
Microsoft Edge WebView2 at runtime. A source checkout does not contain generated
frontend output or native binaries. See [the project README](../README.md),
[the product plan](PLAN.md), [Windows first use](FIRST-USE.md), and
[latency boundaries](LATENCY.md). License and additional permission are preserved
in [LICENSE](../LICENSE) and [LICENSE_EXCEPTION](../LICENSE_EXCEPTION).
