use huddletalk_application::RoomApplicationService;
use huddletalk_infrastructure::InMemoryRoomRepository;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let room_repository = InMemoryRoomRepository::with_seed_data()?;
    let room_service = RoomApplicationService::new(room_repository);
    let rooms = room_service.list_rooms();

    println!(
        "huddletalk-backend started with {} preloaded rooms",
        rooms.len()
    );
    for room in rooms {
        println!("- {} (capacity: {})", room.room_id, room.capacity);
    }
    Ok(())
}
