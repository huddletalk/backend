mod sfu;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{State, ws::WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use huddletalk_application::RoomApplicationService;
use huddletalk_infrastructure::InMemoryRoomRepository;
use tracing::info;

use crate::sfu::{Sfu, handle_signal_socket};

#[derive(Clone)]
struct AppState {
    room_repository: InMemoryRoomRepository,
    sfu: Arc<Sfu>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "huddletalk_backend=info,tower_http=info".to_owned()),
        )
        .init();

    let room_repository = InMemoryRoomRepository::with_seed_data()
        .context("failed to seed in-memory room repository")?;
    let room_service = RoomApplicationService::new(room_repository.clone());
    let rooms = room_service.list_rooms();
    info!("loaded {} rooms from startup data", rooms.len());
    for room in &rooms {
        info!("room '{}' capacity {}", room.room_id, room.capacity);
    }

    let state = AppState {
        room_repository,
        sfu: Arc::new(Sfu::new().await.context("failed to initialize SFU")?),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/rooms", get(list_rooms))
        .route("/signal", get(signal))
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:8080"
        .parse()
        .context("invalid backend bind address")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind backend listener")?;
    info!("huddletalk backend listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("backend server terminated unexpectedly")?;

    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn list_rooms(State(state): State<AppState>) -> Json<Vec<RoomResponse>> {
    let room_service = RoomApplicationService::new(state.room_repository.clone());
    let rooms = room_service
        .list_rooms()
        .into_iter()
        .map(|room| RoomResponse {
            room_id: room.room_id,
            capacity: room.capacity,
        })
        .collect();
    Json(rooms)
}

async fn signal(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_signal_socket(state.sfu, socket))
}
#[derive(serde::Serialize)]
struct RoomResponse {
    room_id: String,
    capacity: usize,
}
