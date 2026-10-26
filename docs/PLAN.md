# RustLanBus — current Windows product plan

> Source status — 2026-10-26: this plan records the Windows target behavior and implementation boundaries. Source coverage and compilation establish implementation/build facts; actual product acceptance is separate. See [ARCHITECTURE.md](ARCHITECTURE.md).

English | [简体中文](PLAN.zh-CN.md) | [繁體中文](PLAN.zh-TW.md) | [日本語](PLAN.ja.md)

Updated: 2026-10-26. Read the [architecture](ARCHITECTURE.md) for source ownership and data flow. This plan describes agreed product targets and current implementation boundaries.

## Purpose and completion boundary

Collect original audio from several LAN computers onto a Windows computer connected to headphones or speakers. Preserve significant PCM precision available at acquisition and minimize avoidable latency. Necessary clock correction, mixing and destination speaker adaptation are explicit parts of playback. There is no fixed millisecond target.

The source connects the agreed features through the shared controller. The 2026-10-26 completion work includes effective settings bundles and migration, card CRUD, truthful per-route state, idle acquisition, resource retirement, smooth gain/recovery boundaries and simultaneous IPv4/IPv6 paths. Focused compilation supports implementation; the owner performs all actual startup, interface, audio and network testing. No current build or archive establishes whole-product completion. Historical archives predate this batch.

## Application structure

- Tauri 2 supplies the Windows desktop shell, window and system tray. React 19 and TypeScript render the local interface in the installed Microsoft Edge WebView2 runtime. No hosted website is required.
- Rust owns configuration, rooms, rules, capture, transport, clocks and playback. Audio buffers do not pass through JavaScript.
- Desktop and formal headless CLI use the same `RunningParticipant` controller and commands. One configuration has one running GUI or CLI instance. The CLI does not remotely control an open desktop.
- Acquisition, network delivery and playback destinations have independent ownership. Each resolved output device has one playback owner and clock.

## Agreed target behavior

### Rooms and members

- One active room supports several independent members. Each device supplies its own name, fixed while joined.
- A Windows creator or OpenWrt service supplies the room directory and peer addresses. A Windows creator leaving or switching rooms stops its contact service. There is no governing host permission matrix, kick/approval layer or automatic host migration.
- Passwords prevent accidental entry. Audio directions are direct P2P. Established direct audio continues after contact-service loss while directory recovery runs in the background.
- Saved cards and discovered rooms are separate. A unique exact name is selected first. Duplicate names use an explicitly entered IP when it uniquely matches, otherwise the user chooses. With no discovered name, an entered address is probed and its returned name must match. Strict automatic entry requires the entered name and IP to correspond.
- A direct address reporting another room name is a target error requiring correction. Background waiting is offered only for temporary absence or contact failure. Automatic networking binds both address families and keeps healthy per-peer paths. A strict interface or explicit-address restriction never silently falls back. An explicitly loopback-bound session defaults to local address entry without LAN browsing. Discovery failure does not stop an available TCP directory; discovery recovers independently. An explicit discovery override retains its supplied scope.
- A successful unknown-room join saves a card. Discovery never fills an omitted saved IP. Reopening restores saved room intent; explicit Leave disables automatic re-entry.

### Sources and directions

- No receiver and no local monitor closes actual native acquisition while preserving peer control, keepalive and publication intent. Existing subscriptions wake acquisition without waiting for a new connection.
- System sound and microphone are independent originals. Defaults: system publication on, microphone publication off, local monitoring off.
- System sound is loopback capture from the selected playback endpoint, commonly an installed virtual speaker. A separate physical output carries local monitoring and received audio. RustLanBus guides setup and supplies no custom kernel driver.
- Default rules allow sending to everyone and receive from nobody. A-to-B requires A to allow sending and B to select reception. Unselected directions carry no audio.
- Listening to a member covers its current and later sources. Expanded controls can close a source role. Local monitoring is independent of room membership. Ordinary clients publish local originals only.

### Rules and persistence

- Editable JSON holds global rules and one profile per room. Global rules run first, room rules second. Later matching assignments override each specified field independently.
- Conditions explicitly match name, IP or both. Name-only operations affect all same-name devices. No IP or persistent device identity is inferred.
- Room actions update and save room rules. Source directions, output targets and partial mix fields retain the same explicit scope.
- The interface shows deciding-rule origins and per-file saved, pending, external-change and write-failure states. External replacements are preserved. Reload applies file edits; clean globals are also reread on room entry.
- Cards can be edited and deleted through the shared owner with external-change revision checks; the active card must be left first.
- One readable JSON settings bundle exports effective local/global/room values including unsaved changes. Explicit restore/copy import previews replacement, checks source/destination revisions and retains a recovery directory. Copy creates new identities, clears local audio/network bindings and disables auto-entry/monitoring/login intent; Main routes remain, fixed targets are removed and listed, and fixed-only routes stay empty.
- Unsaved globals remain active across room switches until explicit Reload. Leaving or switching discards unsaved room changes after a visible warning. Export can rescue unsaved values without changing their original files. Explicit Quit identifies unsaved files and offers return or discard.

### Formats, playback and outputs

- Native integer PCM16/24/32 and F32/F64 share a precision and speaker-mask descriptor. Transport retains the declared original representation; working samples use F64. Current media wire: RLC5/RLP5. Directory JSON version2 carries bounded alternatives on one UDP port. Both directory and return implementations must be updated with the clients. Earlier development peers must update together.
- Mono, stereo and common 5.1/7.1 layouts are supported. Microsoft-published coefficients adapt supported destinations in the existing renderer. Unsupported combinations report an issue per source/output while original transport and other outputs continue.
- Each source can feed several outputs. `main` follows the chosen main device; fixed endpoint choices remain fixed. Missing fixed devices wait for recovery without fallback. Aliases of the same endpoint render once.
- Each output shows its own source phases, device/format errors, levels and overload. A submitted/running indication describes software progress; it does not claim audible hardware output. Missing/stale interface updates are visible.
- Detached full DSP lanes are retired at the cold control boundary. Short gain/mute/recovery ramps share the existing renderer and add no delayed PCM queue.
- Sources sharing an output are mixed there. Secondary controls provide source gain, mute, solo, pan and master volume, with level/overload information. The optional limiter defaults off. Missed old audio is discarded so recovery returns to current sound.

### Interface, CLI and Windows integration

- Rooms and members are primary. Source details, output routing, mix, rules and diagnostics are secondary. First use includes local configuration and audio-route guidance.
- The normal window has a taskbar button. Closing hides it and keeps the application running. The system tray restores the window and offers Exit; explicit Exit stops background owners.
- The formal CLI covers configuration creation/inspection, rooms, devices, sources, rules, outputs, mix, return diagnostics and shutdown. Run has no default time limit and accepts human commands or command JSON.
- Windows executables require administrator privileges. Optional elevated login startup defaults off. Explicit firewall allow/refresh/remove manages only this executable's inbound TCP/UDP rules for the local subnet.

### Optional virtual return

- A return-capable room service exposes one ordinary virtual member per requesting computer. It returns only that computer's originals through the normal peer protocol and needs no audio device or playback buffer.
- Start visibly enables and saves required room directions, preserving microphone publication and output choices. Stop removes the member and retains ordinary saved rules.
- Packet round-trip and capture-to-return reception observations have named boundaries. They do not represent sound-to-ear latency. This remains an optional normal-use diagnostic.

## Remaining closeout and evidence

The current source connects settings migration, card editing, per-route state, demand-driven acquisition, resource retirement, smoothing and simultaneous IPv4/IPv6 paths. Commands, versions and multilingual usage documentation are aligned in the source. The OpenWrt integration now uses the application's independent native package, GL Tools page and own CGI with the official administrator session; it does not revive the retired gateway controller. Ordinary updates preserve configuration and actual runtime/autostart intent without altering the proxy/network backend. [OpenWrt management](../services/openwrt-admin/README.md) records the local command boundary.

The owner operates all actual tests. Do not launch applications, automate the interface, execute tests or probe real audio/network devices without a new explicit request. Necessary focused compilation and source inspection remain implementation aids. Report consequential feature or architecture choices before dependent implementation.

The framework application/tray icon remains; a dedicated icon has no approved design. No Windows installer has been agreed. Existing portable archives are historical candidates and require explicit replacement after source assembly. First-use and delay boundaries are documented in [FIRST-USE.md](FIRST-USE.md) and [LATENCY.md](LATENCY.md).

macOS and Arch adapters follow Windows. SonoBus is a design/code reference; interoperability is outside scope. Recording, hotkeys, internet/NAT operation, encryption and custom Windows driver development are outside the agreed Windows scope.

## Public source and license

Public source includes generic interfaces and documentation-only examples. See the [architecture](ARCHITECTURE.md) and preserve both [GNU GPL v3](../LICENSE) and its [additional permission](../LICENSE_EXCEPTION).
