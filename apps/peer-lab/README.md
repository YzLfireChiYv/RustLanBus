# Peer lab

This is the device-free first-stage entry point for RustLanBus's new peer core. It uses real UDP sockets, whole-state contact and finite F32 sample fixtures. It opens no audio device and outputs no audible sound. It is a development tool; its polling loop and allocating packet API are outside a realtime audio callback.

Run the short local scenario:

    cargo run -p rustlanbus-peer-lab -- demo

The scenario starts one peer before its target UDP port opens, establishes both directed permissions, transmits known samples, closes only one receive direction, verifies reverse transmission, regrants and restarts a peer under a new run identity. Every received fixture sample is checked by its F32 bits. A passing scenario establishes this local data/control behavior; it supplies no two-PC or acoustic latency measurement.

For independent processes, create two saved configurations with stable identities and opposite bind/peer addresses. New configurations default send/receive off unless the corresponding flags are given:

    cargo run -p rustlanbus-peer-lab -- init target/peer-a.json --id 01010101010101010101010101010101 --peer-id 02020202020202020202020202020202 --bind 127.0.0.1:18461 --peer 127.0.0.1:18462 --send --receive
    cargo run -p rustlanbus-peer-lab -- init target/peer-b.json --id 02020202020202020202020202020202 --peer-id 01010101010101010101010101010101 --bind 127.0.0.1:18462 --peer 127.0.0.1:18461 --send --receive

Run each in its own terminal:

    cargo run -p rustlanbus-peer-lab -- run target/peer-a.json --fixture
    cargo run -p rustlanbus-peer-lab -- run target/peer-b.json --fixture

Commands are send on/off, receive on/off, status, reconnect and quit. Changes persist to the configuration, so reopening restores configured intent and automatically contacts the peer. Add --seconds N for bounded diagnostic runs. Without --fixture, only contact/state runs; --fixture explicitly generates known diagnostic samples and verifies that the configured peer uses the same fixture.

Use a configured LAN bind/peer address for later two-machine contact checks. Address discovery, devices, local monitor/mixer, jitter deadlines, source switching and ASRC belong to following stages. The current controller allows one local format and forward frame timeline per process run. Changing a source device or format still needs the later source-epoch API. Temporary peer-port/network errors are counted and retry continues; genuinely unusable socket errors remain visible.

Contact means the peer transport/state session exists. send_ready and receive_ready describe directed audio permission. These are separate from a real device being available or audio being audible.
