# backend

Tokio + Axum backend for HuddleTalk.

## What it does

- Serves `GET /rooms` from an in-memory room repository (seeded at startup).
- Accepts WebSocket signaling at `GET /signal`.
- Hosts a room-based SFU using mediasoup and forwards Opus audio between peers in the same room.

## Run

```bash
cargo run
```

The server listens on `http://0.0.0.0:8080`.

Run with local Whisper STT enabled:

```bash
HUDDLETALK_WHISPER_MODEL_PATH=/Users/fgos/models/ggml-base.en.bin cargo run
```

Optional transport settings:

- `HUDDLETALK_MEDIASOUP_LISTEN_IP` (default `0.0.0.0`)
- `HUDDLETALK_MEDIASOUP_ANNOUNCED_ADDRESS` (default `10.0.2.2` for Android emulator loopback)

Notes:

- For Android emulator you do **not** need to set `HUDDLETALK_MEDIASOUP_ANNOUNCED_ADDRESS`; default `10.0.2.2` is already used.
- For a real phone on the same Wi-Fi, set `HUDDLETALK_MEDIASOUP_ANNOUNCED_ADDRESS` to your Mac LAN IP:

```bash
HUDDLETALK_MEDIASOUP_ANNOUNCED_ADDRESS=192.168.1.123 HUDDLETALK_WHISPER_MODEL_PATH=/Users/fgos/models/ggml-base.en.bin cargo run
```

## Signaling protocol (JSON over `/signal`)

Each request can include `request_id`; direct responses echo the same `request_id`.

- Client -> server:
  - `{"request_id":1,"type":"join","room_id":"lobby"}`
  - `{"request_id":2,"type":"create_webrtc_transport","direction":"send"}`
  - `{"request_id":3,"type":"connect_webrtc_transport","transport_id":"...","dtls_parameters":{...}}`
  - `{"request_id":4,"type":"produce","transport_id":"...","kind":"audio","rtp_parameters":{...}}`
  - `{"request_id":5,"type":"consume","transport_id":"...","producer_id":"...","rtp_capabilities":{...}}`
  - `{"request_id":6,"type":"resume_consumer","consumer_id":"..."}`
- Server -> client:
  - `{"request_id":1,"type":"joined","peer_id":"...","room_id":"lobby","router_rtp_capabilities":{...},"existing_producer_ids":[]}`
  - `{"request_id":2,"type":"webrtc_transport_created","direction":"send","transport_id":"...","ice_parameters":{...},"ice_candidates":[...],"dtls_parameters":{...}}`
  - `{"request_id":3,"type":"transport_connected","transport_id":"..."}`
  - `{"request_id":4,"type":"produced","producer_id":"..."}`
  - `{"type":"new_producer","peer_id":"...","producer_id":"..."}`
  - `{"request_id":5,"type":"consumed","consumer_id":"...","producer_id":"...","kind":"audio","rtp_parameters":{...}}`
  - `{"request_id":6,"type":"consumer_resumed","consumer_id":"..."}`
  - `{"type":"peer_left","peer_id":"..."}`
  - `{"type":"error","message":"..."}`
