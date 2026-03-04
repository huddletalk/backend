use std::{
    collections::HashMap,
    net::IpAddr,
    num::{NonZeroU8, NonZeroU32},
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, anyhow};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use mediasoup::{
    prelude::*,
    types::{
        rtp_parameters::{RtpHeaderExtension, RtpHeaderExtensionDirection},
        sctp_parameters::SctpParameters,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{self, Duration, MissedTickBehavior};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::transcription::{LocalTranscriber, TrackAudioTranscriber, TranscriptIdentity};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransportDirection {
    Send,
    Recv,
}

#[derive(Debug, Deserialize)]
struct ClientEnvelope {
    request_id: Option<u64>,
    #[serde(flatten)]
    signal: ClientSignal,
}

#[derive(Debug, Clone, Serialize)]
struct ServerEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
    #[serde(flatten)]
    signal: ServerSignal,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientSignal {
    Join {
        room_id: String,
    },
    CreateWebrtcTransport {
        direction: TransportDirection,
    },
    ConnectWebrtcTransport {
        transport_id: String,
        dtls_parameters: DtlsParameters,
    },
    Produce {
        transport_id: String,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
    },
    Consume {
        transport_id: String,
        producer_id: String,
        rtp_capabilities: RtpCapabilities,
    },
    ResumeConsumer {
        consumer_id: String,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerSignal {
    Joined {
        peer_id: String,
        room_id: String,
        router_rtp_capabilities: RtpCapabilitiesFinalized,
        existing_producer_ids: Vec<String>,
    },
    WebrtcTransportCreated {
        direction: TransportDirection,
        transport_id: String,
        ice_parameters: IceParameters,
        ice_candidates: Vec<IceCandidate>,
        dtls_parameters: DtlsParameters,
        sctp_parameters: Option<SctpParameters>,
    },
    TransportConnected {
        transport_id: String,
    },
    Produced {
        producer_id: String,
    },
    NewProducer {
        peer_id: String,
        producer_id: String,
    },
    Consumed {
        consumer_id: String,
        producer_id: String,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
    },
    ConsumerResumed {
        consumer_id: String,
    },
    PeerLeft {
        peer_id: String,
    },
    Error {
        message: String,
    },
    Pong,
}

#[derive(Clone)]
struct PeerTransport {
    direction: TransportDirection,
    transport: WebRtcTransport,
}

struct PeerState {
    id: String,
    room_id: String,
    signal_tx: mpsc::UnboundedSender<ServerEnvelope>,
    transports: RwLock<HashMap<String, PeerTransport>>,
    producers: RwLock<HashMap<String, Producer>>,
    consumers: RwLock<HashMap<String, Consumer>>,
    stt_consumers: RwLock<HashMap<String, Consumer>>,
}

#[derive(Default)]
struct RoomState {
    peers: HashMap<String, Arc<PeerState>>,
    producer_owners: HashMap<String, String>,
}

pub struct Sfu {
    _worker_manager: WorkerManager,
    _worker: Worker,
    router: Router,
    direct_transport: DirectTransport,
    rooms: RwLock<HashMap<String, RoomState>>,
    listen_ip: IpAddr,
    announced_address: Option<String>,
    transcriber: Option<Arc<LocalTranscriber>>,
}

const PRODUCER_STATS_LOG_INTERVAL_SECONDS: u64 = 5;

impl Sfu {
    pub async fn new() -> anyhow::Result<Self> {
        let worker_manager = WorkerManager::new();
        let worker = worker_manager
            .create_worker(WorkerSettings::default())
            .await
            .context("failed to create mediasoup worker")?;
        let router = worker
            .create_router(RouterOptions::new(media_codecs()))
            .await
            .context("failed to create mediasoup router")?;
        let direct_transport = router
            .create_direct_transport(DirectTransportOptions::default())
            .await
            .context("failed to create mediasoup direct transport")?;
        let transcriber = LocalTranscriber::from_env()
            .context("failed to initialize local whisper transcriber")?;

        let listen_ip = std::env::var("HUDDLETALK_MEDIASOUP_LISTEN_IP")
            .ok()
            .and_then(|raw| raw.parse::<IpAddr>().ok())
            .unwrap_or_else(|| "0.0.0.0".parse().expect("hardcoded listen IP is valid"));
        let announced_address = std::env::var("HUDDLETALK_MEDIASOUP_ANNOUNCED_ADDRESS")
            .ok()
            .or_else(|| Some("10.0.2.2".to_owned()));

        info!(
            "initialized mediasoup router (listen_ip={}, announced_address={})",
            listen_ip,
            announced_address.as_deref().unwrap_or("<none>")
        );

        Ok(Self {
            _worker_manager: worker_manager,
            _worker: worker,
            router,
            direct_transport,
            rooms: RwLock::new(HashMap::new()),
            listen_ip,
            announced_address,
            transcriber,
        })
    }

    async fn create_peer(
        &self,
        room_id: String,
        signal_tx: mpsc::UnboundedSender<ServerEnvelope>,
    ) -> anyhow::Result<(Arc<PeerState>, Vec<String>)> {
        let peer_id = Uuid::new_v4().to_string();
        let peer = Arc::new(PeerState {
            id: peer_id.clone(),
            room_id: room_id.clone(),
            signal_tx,
            transports: RwLock::new(HashMap::new()),
            producers: RwLock::new(HashMap::new()),
            consumers: RwLock::new(HashMap::new()),
            stt_consumers: RwLock::new(HashMap::new()),
        });

        let existing_producer_ids = {
            let mut rooms = self.rooms.write().await;
            let room = rooms.entry(room_id).or_default();
            let existing = room.producer_owners.keys().cloned().collect::<Vec<_>>();
            room.peers.insert(peer_id, peer.clone());
            existing
        };

        Ok((peer, existing_producer_ids))
    }

    async fn create_webrtc_transport(
        &self,
        peer: Arc<PeerState>,
        direction: TransportDirection,
    ) -> anyhow::Result<WebRtcTransport> {
        let mut transport_options =
            WebRtcTransportOptions::new(WebRtcTransportListenInfos::new(ListenInfo {
                protocol: Protocol::Udp,
                ip: self.listen_ip,
                announced_address: self.announced_address.clone(),
                expose_internal_ip: false,
                port: None,
                port_range: None,
                flags: None,
                send_buffer_size: None,
                recv_buffer_size: None,
            }));
        transport_options.enable_tcp = true;

        let transport = self
            .router
            .create_webrtc_transport(transport_options)
            .await
            .context("failed to create mediasoup WebRTC transport")?;

        peer.transports.write().await.insert(
            transport.id().to_string(),
            PeerTransport {
                direction,
                transport: transport.clone(),
            },
        );

        Ok(transport)
    }

    async fn connect_webrtc_transport(
        &self,
        peer: Arc<PeerState>,
        transport_id: &str,
        dtls_parameters: DtlsParameters,
    ) -> anyhow::Result<()> {
        let peer_transport = peer
            .transports
            .read()
            .await
            .get(transport_id)
            .cloned()
            .ok_or_else(|| anyhow!("transport '{}' not found", transport_id))?;

        peer_transport
            .transport
            .connect(WebRtcTransportRemoteParameters { dtls_parameters })
            .await
            .context("failed to connect mediasoup WebRTC transport")?;

        Ok(())
    }

    async fn produce(
        &self,
        peer: Arc<PeerState>,
        transport_id: &str,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
    ) -> anyhow::Result<(String, Vec<Arc<PeerState>>)> {
        let peer_transport = peer
            .transports
            .read()
            .await
            .get(transport_id)
            .cloned()
            .ok_or_else(|| anyhow!("transport '{}' not found", transport_id))?;
        if peer_transport.direction != TransportDirection::Send {
            return Err(anyhow!(
                "transport '{}' is not a send transport",
                transport_id
            ));
        }

        let producer_options = ProducerOptions::new(kind, rtp_parameters);
        let producer = peer_transport
            .transport
            .produce(producer_options)
            .await
            .context("failed to create mediasoup producer")?;
        let producer_id = producer.id().to_string();
        self.spawn_producer_stats_logger(
            peer.room_id.clone(),
            peer.id.clone(),
            producer_id.clone(),
            producer.downgrade(),
        );

        peer.producers
            .write()
            .await
            .insert(producer_id.clone(), producer.clone());
        self.maybe_start_transcription_for_producer(peer.clone(), &producer_id, producer)
            .await;

        let peers_to_notify = {
            let mut rooms = self.rooms.write().await;
            let room = rooms
                .get_mut(&peer.room_id)
                .ok_or_else(|| anyhow!("room '{}' not found", peer.room_id))?;
            room.producer_owners
                .insert(producer_id.clone(), peer.id.clone());

            room.peers
                .iter()
                .filter_map(|(peer_id, other_peer)| {
                    if *peer_id == peer.id {
                        None
                    } else {
                        Some(other_peer.clone())
                    }
                })
                .collect::<Vec<_>>()
        };

        Ok((producer_id, peers_to_notify))
    }

    async fn maybe_start_transcription_for_producer(
        &self,
        peer: Arc<PeerState>,
        producer_id: &str,
        producer: Producer,
    ) {
        let Some(transcriber) = self.transcriber.clone() else {
            return;
        };

        if producer.kind() != MediaKind::Audio {
            return;
        }

        let Some(opus_channels) = opus_channel_count(producer.rtp_parameters()) else {
            warn!(
                "skipping transcription room='{}' peer='{}' producer='{}' reason='opus codec not found'",
                peer.room_id, peer.id, producer_id
            );
            return;
        };

        let rtp_capabilities = rtp_capabilities_from_producer(&producer);
        if !self.router.can_consume(&producer.id(), &rtp_capabilities) {
            warn!(
                "skipping transcription room='{}' peer='{}' producer='{}' reason='router cannot consume producer'",
                peer.room_id, peer.id, producer_id
            );
            return;
        }

        let direct_consumer = match self
            .direct_transport
            .consume(ConsumerOptions::new(producer.id(), rtp_capabilities))
            .await
        {
            Ok(consumer) => consumer,
            Err(err) => {
                warn!(
                    "failed to create transcription consumer room='{}' peer='{}' producer='{}': {}",
                    peer.room_id, peer.id, producer_id, err
                );
                return;
            }
        };

        let identity = TranscriptIdentity {
            room_id: peer.room_id.clone(),
            peer_id: peer.id.clone(),
            track_id: producer_id.to_owned(),
        };
        let track_transcriber = match TrackAudioTranscriber::new(
            transcriber,
            identity,
            opus_channels,
        ) {
            Ok(track_transcriber) => track_transcriber,
            Err(err) => {
                warn!(
                    "failed to initialize transcription decoder room='{}' peer='{}' producer='{}': {}",
                    peer.room_id, peer.id, producer_id, err
                );
                return;
            }
        };
        let track_transcriber = Arc::new(StdMutex::new(track_transcriber));

        let track_transcriber_for_rtp = Arc::clone(&track_transcriber);
        let on_rtp_handler = direct_consumer.on_rtp(move |rtp_packet| {
            let Ok(mut transcriber) = track_transcriber_for_rtp.lock() else {
                warn!("failed to lock transcription state while processing RTP packet");
                return;
            };
            transcriber.ingest_rtp_packet(rtp_packet);
        });
        on_rtp_handler.detach();

        let track_transcriber_for_close = Arc::clone(&track_transcriber);
        let room_id_for_close = peer.room_id.clone();
        let peer_id_for_close = peer.id.clone();
        let producer_id_for_close = producer_id.to_owned();
        let on_close_handler = direct_consumer.on_close(move || {
            let Ok(mut transcriber) = track_transcriber_for_close.lock() else {
                warn!("failed to lock transcription state while finishing track");
                return;
            };
            transcriber.finish();
            info!(
                "stopped transcription consumer room='{}' peer='{}' producer='{}'",
                room_id_for_close, peer_id_for_close, producer_id_for_close
            );
        });
        on_close_handler.detach();

        peer.stt_consumers
            .write()
            .await
            .insert(producer_id.to_owned(), direct_consumer);
        info!(
            "started transcription consumer room='{}' peer='{}' producer='{}' channels={}",
            peer.room_id, peer.id, producer_id, opus_channels
        );
    }

    fn spawn_producer_stats_logger(
        &self,
        room_id: String,
        peer_id: String,
        producer_id: String,
        weak_producer: WeakProducer,
    ) {
        tokio::spawn(async move {
            let mut interval =
                time::interval(Duration::from_secs(PRODUCER_STATS_LOG_INTERVAL_SECONDS));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            let mut previous_packet_count: Option<u64> = None;
            let mut previous_byte_count: Option<u64> = None;

            info!(
                "started producer stats logger room='{}' peer='{}' producer='{}'",
                room_id, peer_id, producer_id
            );

            loop {
                interval.tick().await;

                let Some(producer) = weak_producer.upgrade() else {
                    info!(
                        "stopped producer stats logger room='{}' peer='{}' producer='{}' (producer dropped)",
                        room_id, peer_id, producer_id
                    );
                    break;
                };

                if producer.closed() {
                    info!(
                        "stopped producer stats logger room='{}' peer='{}' producer='{}' (producer closed)",
                        room_id, peer_id, producer_id
                    );
                    break;
                }

                let stats = match producer.get_stats().await {
                    Ok(stats) => stats,
                    Err(err) => {
                        warn!(
                            "failed to query producer stats room='{}' peer='{}' producer='{}': {}",
                            room_id, peer_id, producer_id, err
                        );
                        continue;
                    }
                };

                if stats.is_empty() {
                    info!(
                        "producer stats room='{}' peer='{}' producer='{}' streams=0",
                        room_id, peer_id, producer_id
                    );
                    continue;
                }

                let stream_count = stats.len();
                let packet_count: u64 = stats.iter().map(|stat| stat.packet_count).sum();
                let byte_count: u64 = stats.iter().map(|stat| stat.byte_count).sum();
                let bitrate_bps: u64 = stats.iter().map(|stat| stat.bitrate as u64).sum();
                let packets_lost: u64 = stats.iter().map(|stat| stat.packets_lost).sum();

                let delta_packets = previous_packet_count
                    .map(|previous| packet_count.saturating_sub(previous))
                    .unwrap_or(0);
                let delta_bytes = previous_byte_count
                    .map(|previous| byte_count.saturating_sub(previous))
                    .unwrap_or(0);

                previous_packet_count = Some(packet_count);
                previous_byte_count = Some(byte_count);

                info!(
                    "producer stats room='{}' peer='{}' producer='{}' streams={} packets={} (+{}) bytes={} (+{}) bitrate_bps={} packets_lost={}",
                    room_id,
                    peer_id,
                    producer_id,
                    stream_count,
                    packet_count,
                    delta_packets,
                    byte_count,
                    delta_bytes,
                    bitrate_bps,
                    packets_lost
                );
            }
        });
    }

    async fn consume(
        &self,
        peer: Arc<PeerState>,
        transport_id: &str,
        producer_id: &str,
        rtp_capabilities: RtpCapabilities,
    ) -> anyhow::Result<Consumer> {
        let peer_transport = peer
            .transports
            .read()
            .await
            .get(transport_id)
            .cloned()
            .ok_or_else(|| anyhow!("transport '{}' not found", transport_id))?;
        if peer_transport.direction != TransportDirection::Recv {
            return Err(anyhow!(
                "transport '{}' is not a recv transport",
                transport_id
            ));
        }

        let producer_id_typed = producer_id
            .parse::<ProducerId>()
            .context("invalid producer ID format")?;
        let can_consume = self
            .router
            .can_consume(&producer_id_typed, &rtp_capabilities);
        if !can_consume {
            return Err(anyhow!(
                "router cannot consume producer '{}' with provided RTP capabilities",
                producer_id
            ));
        }

        let mut consumer_options = ConsumerOptions::new(producer_id_typed, rtp_capabilities);
        consumer_options.paused = true;
        let consumer = peer_transport
            .transport
            .consume(consumer_options)
            .await
            .context("failed to create mediasoup consumer")?;

        peer.consumers
            .write()
            .await
            .insert(consumer.id().to_string(), consumer.clone());

        Ok(consumer)
    }

    async fn resume_consumer(&self, peer: Arc<PeerState>, consumer_id: &str) -> anyhow::Result<()> {
        let consumer = peer
            .consumers
            .read()
            .await
            .get(consumer_id)
            .cloned()
            .ok_or_else(|| anyhow!("consumer '{}' not found", consumer_id))?;

        consumer
            .resume()
            .await
            .context("failed to resume mediasoup consumer")?;
        Ok(())
    }

    async fn room_has_producer(&self, room_id: &str, producer_id: &str) -> bool {
        let rooms = self.rooms.read().await;
        rooms
            .get(room_id)
            .is_some_and(|room| room.producer_owners.contains_key(producer_id))
    }

    async fn remove_peer(&self, room_id: &str, peer_id: &str) {
        let removed_peer = {
            let mut rooms = self.rooms.write().await;
            let Some(room) = rooms.get_mut(room_id) else {
                return;
            };
            room.peers.remove(peer_id)
        };
        let Some(removed_peer) = removed_peer else {
            return;
        };

        let removed_producer_ids = removed_peer
            .producers
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        let (remaining_peers, room_empty) = {
            let mut rooms = self.rooms.write().await;
            let Some(room) = rooms.get_mut(room_id) else {
                return;
            };

            for producer_id in &removed_producer_ids {
                room.producer_owners.remove(producer_id);
            }

            let peers = room.peers.values().cloned().collect::<Vec<_>>();
            let room_empty = room.peers.is_empty();
            if room_empty {
                rooms.remove(room_id);
            }

            (peers, room_empty)
        };

        for peer in remaining_peers {
            let _ = peer.signal_tx.send(ServerEnvelope {
                request_id: None,
                signal: ServerSignal::PeerLeft {
                    peer_id: peer_id.to_owned(),
                },
            });
        }

        info!("peer '{}' left room '{}'", peer_id, room_id);
        if room_empty {
            info!("room '{}' is empty and was removed", room_id);
        }
    }
}

pub async fn handle_signal_socket(sfu: Arc<Sfu>, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerEnvelope>();

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

    let mut current_peer: Option<Arc<PeerState>> = None;

    while let Some(Ok(message)) = ws_receiver.next().await {
        match message {
            Message::Text(payload) => {
                let envelope = match serde_json::from_str::<ClientEnvelope>(&payload) {
                    Ok(envelope) => envelope,
                    Err(err) => {
                        let _ = outbound_tx.send(ServerEnvelope {
                            request_id: None,
                            signal: ServerSignal::Error {
                                message: format!("invalid signal payload: {err}"),
                            },
                        });
                        continue;
                    }
                };

                let request_id = envelope.request_id;
                let send_error = |message: String, tx: &mpsc::UnboundedSender<ServerEnvelope>| {
                    let _ = tx.send(ServerEnvelope {
                        request_id,
                        signal: ServerSignal::Error { message },
                    });
                };

                match envelope.signal {
                    ClientSignal::Join { room_id } => {
                        if current_peer.is_some() {
                            send_error("peer is already joined to a room".to_owned(), &outbound_tx);
                            continue;
                        }

                        match sfu.create_peer(room_id.clone(), outbound_tx.clone()).await {
                            Ok((peer, existing_producer_ids)) => {
                                current_peer = Some(peer.clone());
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::Joined {
                                        peer_id: peer.id.clone(),
                                        room_id: room_id.clone(),
                                        router_rtp_capabilities: sfu.router.rtp_capabilities(),
                                        existing_producer_ids,
                                    },
                                });
                            }
                            Err(err) => {
                                send_error(format!("failed to join room: {err}"), &outbound_tx);
                            }
                        }
                    }
                    ClientSignal::CreateWebrtcTransport { direction } => {
                        let Some(peer) = current_peer.clone() else {
                            send_error(
                                "join a room before creating transports".to_owned(),
                                &outbound_tx,
                            );
                            continue;
                        };

                        match sfu.create_webrtc_transport(peer, direction).await {
                            Ok(transport) => {
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::WebrtcTransportCreated {
                                        direction,
                                        transport_id: transport.id().to_string(),
                                        ice_parameters: transport.ice_parameters().clone(),
                                        ice_candidates: transport.ice_candidates().clone(),
                                        dtls_parameters: transport.dtls_parameters(),
                                        sctp_parameters: transport.sctp_parameters(),
                                    },
                                });
                            }
                            Err(err) => {
                                send_error(
                                    format!("failed to create transport: {err}"),
                                    &outbound_tx,
                                );
                            }
                        }
                    }
                    ClientSignal::ConnectWebrtcTransport {
                        transport_id,
                        dtls_parameters,
                    } => {
                        let Some(peer) = current_peer.clone() else {
                            send_error(
                                "join a room before connecting transports".to_owned(),
                                &outbound_tx,
                            );
                            continue;
                        };

                        match sfu
                            .connect_webrtc_transport(peer, &transport_id, dtls_parameters)
                            .await
                        {
                            Ok(()) => {
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::TransportConnected { transport_id },
                                });
                            }
                            Err(err) => {
                                send_error(
                                    format!("failed to connect transport: {err}"),
                                    &outbound_tx,
                                );
                            }
                        }
                    }
                    ClientSignal::Produce {
                        transport_id,
                        kind,
                        rtp_parameters,
                    } => {
                        let Some(peer) = current_peer.clone() else {
                            send_error("join a room before producing".to_owned(), &outbound_tx);
                            continue;
                        };

                        match sfu
                            .produce(peer.clone(), &transport_id, kind, rtp_parameters)
                            .await
                        {
                            Ok((producer_id, peers_to_notify)) => {
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::Produced {
                                        producer_id: producer_id.clone(),
                                    },
                                });
                                for other_peer in peers_to_notify {
                                    let _ = other_peer.signal_tx.send(ServerEnvelope {
                                        request_id: None,
                                        signal: ServerSignal::NewProducer {
                                            peer_id: peer.id.clone(),
                                            producer_id: producer_id.clone(),
                                        },
                                    });
                                }
                            }
                            Err(err) => {
                                send_error(format!("failed to produce: {err}"), &outbound_tx);
                            }
                        }
                    }
                    ClientSignal::Consume {
                        transport_id,
                        producer_id,
                        rtp_capabilities,
                    } => {
                        let Some(peer) = current_peer.clone() else {
                            send_error("join a room before consuming".to_owned(), &outbound_tx);
                            continue;
                        };

                        if !sfu.room_has_producer(&peer.room_id, &producer_id).await {
                            send_error(
                                format!("producer '{}' does not exist in room", producer_id),
                                &outbound_tx,
                            );
                            continue;
                        }

                        match sfu
                            .consume(peer, &transport_id, &producer_id, rtp_capabilities)
                            .await
                        {
                            Ok(consumer) => {
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::Consumed {
                                        consumer_id: consumer.id().to_string(),
                                        producer_id: consumer.producer_id().to_string(),
                                        kind: consumer.kind(),
                                        rtp_parameters: consumer.rtp_parameters().clone(),
                                    },
                                });
                            }
                            Err(err) => {
                                send_error(format!("failed to consume: {err}"), &outbound_tx);
                            }
                        }
                    }
                    ClientSignal::ResumeConsumer { consumer_id } => {
                        let Some(peer) = current_peer.clone() else {
                            send_error(
                                "join a room before resuming consumers".to_owned(),
                                &outbound_tx,
                            );
                            continue;
                        };

                        match sfu.resume_consumer(peer, &consumer_id).await {
                            Ok(()) => {
                                let _ = outbound_tx.send(ServerEnvelope {
                                    request_id,
                                    signal: ServerSignal::ConsumerResumed { consumer_id },
                                });
                            }
                            Err(err) => {
                                send_error(
                                    format!("failed to resume consumer: {err}"),
                                    &outbound_tx,
                                );
                            }
                        }
                    }
                    ClientSignal::Ping => {
                        let _ = outbound_tx.send(ServerEnvelope {
                            request_id,
                            signal: ServerSignal::Pong,
                        });
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    if let Some(peer) = current_peer {
        sfu.remove_peer(&peer.room_id, &peer.id).await;
    } else {
        warn!("signal socket closed before peer joined a room");
    }
}

fn opus_channel_count(rtp_parameters: &RtpParameters) -> Option<u16> {
    rtp_parameters.codecs.iter().find_map(|codec| {
        if let RtpCodecParameters::Audio {
            mime_type,
            channels,
            ..
        } = codec
        {
            if *mime_type == MimeTypeAudio::Opus {
                return Some(channels.get() as u16);
            }
        }

        None
    })
}

fn rtp_capabilities_from_producer(producer: &Producer) -> RtpCapabilities {
    let kind = producer.kind();
    let consumable_rtp_parameters = producer.consumable_rtp_parameters();

    let codecs = consumable_rtp_parameters
        .codecs
        .iter()
        .map(|codec| match codec {
            RtpCodecParameters::Audio {
                mime_type,
                payload_type,
                clock_rate,
                channels,
                parameters,
                rtcp_feedback,
            } => RtpCodecCapability::Audio {
                mime_type: *mime_type,
                preferred_payload_type: Some(*payload_type),
                clock_rate: *clock_rate,
                channels: *channels,
                parameters: parameters.clone(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            RtpCodecParameters::Video {
                mime_type,
                payload_type,
                clock_rate,
                parameters,
                rtcp_feedback,
            } => RtpCodecCapability::Video {
                mime_type: *mime_type,
                preferred_payload_type: Some(*payload_type),
                clock_rate: *clock_rate,
                parameters: parameters.clone(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
        })
        .collect::<Vec<_>>();

    let header_extensions = consumable_rtp_parameters
        .header_extensions
        .iter()
        .map(|header_extension| RtpHeaderExtension {
            kind,
            uri: header_extension.uri,
            preferred_id: header_extension.id,
            preferred_encrypt: header_extension.encrypt,
            direction: RtpHeaderExtensionDirection::SendRecv,
        })
        .collect::<Vec<_>>();

    RtpCapabilities {
        codecs,
        header_extensions,
    }
}

fn media_codecs() -> Vec<RtpCodecCapability> {
    vec![RtpCodecCapability::Audio {
        mime_type: MimeTypeAudio::Opus,
        preferred_payload_type: None,
        clock_rate: NonZeroU32::new(48_000).expect("hardcoded non-zero sample rate"),
        channels: NonZeroU8::new(2).expect("hardcoded non-zero channel count"),
        parameters: RtpCodecParametersParameters::default(),
        rtcp_feedback: vec![],
    }]
}
