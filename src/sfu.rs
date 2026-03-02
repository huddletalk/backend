use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;
use webrtc::{
    api::{
        API, APIBuilder, interceptor_registry::register_default_interceptors,
        media_engine::MediaEngine,
    },
    ice_transport::ice_candidate::RTCIceCandidateInit,
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::{
        track_local::{TrackLocalWriter, track_local_static_rtp::TrackLocalStaticRTP},
        track_remote::TrackRemote,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientSignal {
    Join {
        room_id: String,
    },
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerSignal {
    Joined {
        peer_id: String,
        room_id: String,
    },
    Answer {
        sdp: String,
    },
    Offer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    PeerJoined {
        peer_id: String,
    },
    PeerLeft {
        peer_id: String,
    },
    Error {
        message: String,
    },
    Pong,
}

#[derive(Debug, Default)]
struct RoomState {
    peers: HashMap<String, Arc<Peer>>,
    forwarded_tracks: HashMap<String, Arc<ForwardedTrack>>,
}

#[derive(Debug)]
struct Peer {
    id: String,
    room_id: String,
    pc: Arc<RTCPeerConnection>,
    signal_tx: mpsc::UnboundedSender<ServerSignal>,
}

#[derive(Debug)]
struct ForwardedTrack {
    id: String,
    stream_id: String,
    source_peer_id: String,
    codec: RTCRtpCodecCapability,
    sinks: Arc<RwLock<Vec<Arc<TrackLocalStaticRTP>>>>,
    attached_peers: Arc<RwLock<HashSet<String>>>,
}

pub struct Sfu {
    api: API,
    rooms: RwLock<HashMap<String, RoomState>>,
}

impl Sfu {
    pub async fn new() -> anyhow::Result<Self> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("failed to register default codecs")?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .context("failed to register default interceptors")?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        Ok(Self {
            api,
            rooms: RwLock::new(HashMap::new()),
        })
    }

    async fn create_peer(
        self: &Arc<Self>,
        room_id: String,
        signal_tx: mpsc::UnboundedSender<ServerSignal>,
    ) -> anyhow::Result<Arc<Peer>> {
        let pc = Arc::new(
            self.api
                .new_peer_connection(RTCConfiguration::default())
                .await
                .context("failed to create peer connection")?,
        );

        let peer_id = Uuid::new_v4().to_string();
        let peer = Arc::new(Peer {
            id: peer_id.clone(),
            room_id: room_id.clone(),
            pc: pc.clone(),
            signal_tx: signal_tx.clone(),
        });

        self.register_peer_callbacks(peer.clone());

        let (existing_peers, existing_tracks) = {
            let mut rooms = self.rooms.write().await;
            let room = rooms.entry(room_id.clone()).or_default();
            let peers = room.peers.values().cloned().collect::<Vec<_>>();
            let tracks = room.forwarded_tracks.values().cloned().collect::<Vec<_>>();
            room.peers.insert(peer_id.clone(), peer.clone());
            (peers, tracks)
        };

        for other in &existing_peers {
            let _ = other.signal_tx.send(ServerSignal::PeerJoined {
                peer_id: peer_id.clone(),
            });
            let _ = peer.signal_tx.send(ServerSignal::PeerJoined {
                peer_id: other.id.clone(),
            });
        }

        for track in existing_tracks {
            self.attach_track_to_peer(peer.clone(), track).await?;
        }
        if !existing_peers.is_empty() {
            self.renegotiate_peer(peer.clone()).await?;
        }

        Ok(peer)
    }

    fn register_peer_callbacks(self: &Arc<Self>, peer: Arc<Peer>) {
        let peer_for_ice = peer.clone();
        peer.pc.on_ice_candidate(Box::new(move |candidate| {
            let peer = peer_for_ice.clone();
            Box::pin(async move {
                let Some(candidate) = candidate else {
                    return;
                };
                match candidate.to_json() {
                    Ok(candidate_json) => {
                        let _ = peer.signal_tx.send(ServerSignal::IceCandidate {
                            candidate: candidate_json.candidate,
                            sdp_mid: candidate_json.sdp_mid,
                            sdp_mline_index: candidate_json.sdp_mline_index,
                        });
                    }
                    Err(err) => {
                        error!("failed to serialize ICE candidate: {err}");
                    }
                }
            })
        }));

        let sfu_for_state = self.clone();
        let peer_for_state = peer.clone();
        peer.pc
            .on_peer_connection_state_change(Box::new(move |state| {
                let sfu = sfu_for_state.clone();
                let peer = peer_for_state.clone();
                Box::pin(async move {
                    if matches!(
                        state,
                        RTCPeerConnectionState::Closed
                            | RTCPeerConnectionState::Failed
                            | RTCPeerConnectionState::Disconnected
                    ) {
                        sfu.remove_peer(&peer.room_id, &peer.id).await;
                    }
                })
            }));

        let sfu_for_track = self.clone();
        let peer_for_track = peer;
        let pc_for_track = peer_for_track.pc.clone();
        pc_for_track.on_track(Box::new(move |track, _receiver, _transceiver| {
            let sfu = sfu_for_track.clone();
            let peer = peer_for_track.clone();
            Box::pin(async move {
                if let Err(err) = sfu.publish_track(peer, track).await {
                    error!("failed to publish incoming track: {err}");
                }
            })
        }));
    }

    async fn publish_track(
        self: &Arc<Self>,
        source_peer: Arc<Peer>,
        track: Arc<TrackRemote>,
    ) -> anyhow::Result<()> {
        let track_id = track.id();
        let stream_id = track.stream_id();
        let codec = track.codec().capability;

        let forwarded = Arc::new(ForwardedTrack {
            id: track_id.clone(),
            stream_id,
            source_peer_id: source_peer.id.clone(),
            codec,
            sinks: Arc::new(RwLock::new(Vec::new())),
            attached_peers: Arc::new(RwLock::new(HashSet::new())),
        });

        let destinations = {
            let mut rooms = self.rooms.write().await;
            let room = rooms.entry(source_peer.room_id.clone()).or_default();
            room.forwarded_tracks
                .insert(track_id.clone(), forwarded.clone());
            room.peers
                .iter()
                .filter_map(|(peer_id, peer)| {
                    if *peer_id == source_peer.id {
                        None
                    } else {
                        Some(peer.clone())
                    }
                })
                .collect::<Vec<_>>()
        };

        for destination in destinations {
            self.attach_track_to_peer(destination.clone(), forwarded.clone())
                .await?;
            self.renegotiate_peer(destination).await?;
        }

        let sfu = self.clone();
        let room_id_for_log = source_peer.room_id.clone();
        let source_peer_id_for_log = source_peer.id.clone();
        let track_id_for_log = track_id.clone();
        let codec_for_log = forwarded.codec.mime_type.clone();
        tokio::spawn(async move {
            let mut packets_received: u64 = 0;
            let mut payload_bytes_received: u64 = 0;
            let mut last_receive_log = tokio::time::Instant::now();
            let receive_log_interval = tokio::time::Duration::from_secs(5);

            while let Ok((packet, _)) = track.read_rtp().await {
                packets_received += 1;
                payload_bytes_received += packet.payload.len() as u64;

                if last_receive_log.elapsed() >= receive_log_interval {
                    info!(
                        "receiving RTP room='{}' peer='{}' track='{}' codec='{}' packets={} payload_bytes={}",
                        room_id_for_log,
                        source_peer_id_for_log,
                        track_id_for_log,
                        codec_for_log,
                        packets_received,
                        payload_bytes_received
                    );
                    last_receive_log = tokio::time::Instant::now();
                }

                let sinks = forwarded.sinks.read().await.clone();
                for sink in sinks {
                    if let Err(err) = sink.write_rtp(&packet).await {
                        warn!("failed to forward RTP packet to sink: {err}");
                    }
                }
            }

            sfu.remove_track(&source_peer.room_id, &track_id).await;
            info!(
                "stopped forwarding track '{}' in room '{}'",
                track_id, source_peer.room_id
            );
        });

        Ok(())
    }

    async fn attach_track_to_peer(
        &self,
        destination_peer: Arc<Peer>,
        forwarded_track: Arc<ForwardedTrack>,
    ) -> anyhow::Result<()> {
        {
            let mut attached_peers = forwarded_track.attached_peers.write().await;
            if attached_peers.contains(&destination_peer.id) {
                return Ok(());
            }
            attached_peers.insert(destination_peer.id.clone());
        }

        let local_track = Arc::new(TrackLocalStaticRTP::new(
            forwarded_track.codec.clone(),
            format!("{}-{}", forwarded_track.source_peer_id, forwarded_track.id),
            forwarded_track.stream_id.clone(),
        ));
        let sender = destination_peer
            .pc
            .add_track(local_track.clone())
            .await
            .context("failed to add forwarded track to destination peer")?;

        forwarded_track.sinks.write().await.push(local_track);

        tokio::spawn(async move {
            let mut rtcp_buffer = vec![0u8; 1500];
            while sender.read(&mut rtcp_buffer).await.is_ok() {}
        });

        Ok(())
    }

    async fn renegotiate_peer(&self, peer: Arc<Peer>) -> anyhow::Result<()> {
        let offer = peer
            .pc
            .create_offer(None)
            .await
            .context("failed to create server offer")?;
        peer.pc
            .set_local_description(offer.clone())
            .await
            .context("failed to set local description for server offer")?;
        peer.signal_tx
            .send(ServerSignal::Offer { sdp: offer.sdp })
            .context("failed to deliver renegotiation offer to client")?;
        Ok(())
    }

    async fn apply_offer(&self, peer: Arc<Peer>, sdp: String) -> anyhow::Result<String> {
        let offer = RTCSessionDescription::offer(sdp)
            .context("failed to parse client offer session description")?;
        peer.pc
            .set_remote_description(offer)
            .await
            .context("failed to set remote offer description")?;

        let answer = peer
            .pc
            .create_answer(None)
            .await
            .context("failed to create answer")?;
        peer.pc
            .set_local_description(answer.clone())
            .await
            .context("failed to set local answer description")?;

        Ok(answer.sdp)
    }

    async fn apply_answer(&self, peer: Arc<Peer>, sdp: String) -> anyhow::Result<()> {
        let answer = RTCSessionDescription::answer(sdp)
            .context("failed to parse client answer session description")?;
        peer.pc
            .set_remote_description(answer)
            .await
            .context("failed to set remote answer description")?;
        Ok(())
    }

    async fn add_ice_candidate(
        &self,
        peer: Arc<Peer>,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> anyhow::Result<()> {
        let candidate_init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
        };
        peer.pc
            .add_ice_candidate(candidate_init)
            .await
            .context("failed to add ICE candidate")?;
        Ok(())
    }

    async fn remove_track(&self, room_id: &str, track_id: &str) {
        let mut rooms = self.rooms.write().await;
        if let Some(room) = rooms.get_mut(room_id) {
            room.forwarded_tracks.remove(track_id);
        }
    }

    async fn remove_peer(&self, room_id: &str, peer_id: &str) {
        let (removed_peer, remaining_peers, room_empty) = {
            let mut rooms = self.rooms.write().await;
            let Some(room) = rooms.get_mut(room_id) else {
                return;
            };
            let removed_peer = room.peers.remove(peer_id);
            room.forwarded_tracks
                .retain(|_, track| track.source_peer_id != peer_id);
            let remaining = room.peers.values().cloned().collect::<Vec<_>>();
            let room_empty = room.peers.is_empty();
            if room_empty {
                rooms.remove(room_id);
            }
            (removed_peer, remaining, room_empty)
        };

        if let Some(peer) = removed_peer {
            let _ = peer.pc.close().await;
            for remaining in remaining_peers {
                let _ = remaining.signal_tx.send(ServerSignal::PeerLeft {
                    peer_id: peer_id.to_owned(),
                });
            }
            info!("peer '{peer_id}' left room '{room_id}'");
        }

        if room_empty {
            info!("room '{room_id}' is empty and was removed");
        }
    }
}

pub async fn handle_signal_socket(sfu: Arc<Sfu>, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerSignal>();

    tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            let payload = match serde_json::to_string(&message) {
                Ok(payload) => payload,
                Err(err) => {
                    error!("failed to serialize signal message: {err}");
                    continue;
                }
            };
            if ws_sender.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    let mut current_peer: Option<Arc<Peer>> = None;

    while let Some(Ok(message)) = ws_receiver.next().await {
        match message {
            Message::Text(payload) => {
                let client_signal = match serde_json::from_str::<ClientSignal>(&payload) {
                    Ok(signal) => signal,
                    Err(err) => {
                        let _ = outbound_tx.send(ServerSignal::Error {
                            message: format!("invalid signal payload: {err}"),
                        });
                        continue;
                    }
                };

                match client_signal {
                    ClientSignal::Join { room_id } => {
                        if current_peer.is_some() {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: "peer is already joined to a room".to_owned(),
                            });
                            continue;
                        }

                        match sfu.create_peer(room_id.clone(), outbound_tx.clone()).await {
                            Ok(peer) => {
                                let _ = outbound_tx.send(ServerSignal::Joined {
                                    peer_id: peer.id.clone(),
                                    room_id: room_id.clone(),
                                });
                                current_peer = Some(peer);
                            }
                            Err(err) => {
                                let _ = outbound_tx.send(ServerSignal::Error {
                                    message: format!("failed to join room: {err}"),
                                });
                            }
                        }
                    }
                    ClientSignal::Offer { sdp } => {
                        let Some(peer) = current_peer.clone() else {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: "join a room before sending offer".to_owned(),
                            });
                            continue;
                        };

                        match sfu.apply_offer(peer, sdp).await {
                            Ok(answer_sdp) => {
                                let _ = outbound_tx.send(ServerSignal::Answer { sdp: answer_sdp });
                            }
                            Err(err) => {
                                let _ = outbound_tx.send(ServerSignal::Error {
                                    message: format!("failed to apply offer: {err}"),
                                });
                            }
                        }
                    }
                    ClientSignal::Answer { sdp } => {
                        let Some(peer) = current_peer.clone() else {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: "join a room before sending answer".to_owned(),
                            });
                            continue;
                        };

                        if let Err(err) = sfu.apply_answer(peer, sdp).await {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: format!("failed to apply answer: {err}"),
                            });
                        }
                    }
                    ClientSignal::IceCandidate {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    } => {
                        let Some(peer) = current_peer.clone() else {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: "join a room before sending ICE candidates".to_owned(),
                            });
                            continue;
                        };

                        if let Err(err) = sfu
                            .add_ice_candidate(peer, candidate, sdp_mid, sdp_mline_index)
                            .await
                        {
                            let _ = outbound_tx.send(ServerSignal::Error {
                                message: format!("failed to add ICE candidate: {err}"),
                            });
                        }
                    }
                    ClientSignal::Ping => {
                        let _ = outbound_tx.send(ServerSignal::Pong);
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    if let Some(peer) = current_peer {
        sfu.remove_peer(&peer.room_id, &peer.id).await;
    }
}
