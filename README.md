# backend

Tokio + Axum backend for HuddleTalk.

## What it does

- Serves `GET /rooms` from an in-memory room repository (seeded at startup).
- Accepts WebSocket signaling at `GET /signal`.
- Hosts a room-based SFU that accepts WebRTC peer connections and forwards RTP audio tracks between peers in the same room.

## Run

```bash
cargo run
```

The server listens on `http://0.0.0.0:8080`.

## Signaling protocol (JSON over `/signal`)

- Client -> server:
  - `{"type":"join","room_id":"lobby"}`
  - `{"type":"offer","sdp":"..."}`
  - `{"type":"answer","sdp":"..."}`
  - `{"type":"ice_candidate","candidate":"...","sdp_mid":"0","sdp_mline_index":0}`
- Server -> client:
  - `{"type":"joined","peer_id":"...","room_id":"lobby"}`
  - `{"type":"answer","sdp":"..."}`
  - `{"type":"offer","sdp":"..."}`
  - `{"type":"ice_candidate","candidate":"...","sdp_mid":"0","sdp_mline_index":0}`
  - `{"type":"peer_joined","peer_id":"..."}`
  - `{"type":"peer_left","peer_id":"..."}`
  - `{"type":"error","message":"..."}`
