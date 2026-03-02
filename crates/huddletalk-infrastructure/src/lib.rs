use std::collections::HashMap;

use huddletalk_domain::{DomainError, Room, RoomId, RoomRepository};

#[derive(Debug, Clone, Default)]
pub struct InMemoryRoomRepository {
    rooms: HashMap<RoomId, Room>,
}

impl InMemoryRoomRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_seed_data() -> Result<Self, DomainError> {
        let mut repository = Self::new();
        for (room_id, capacity) in [("lobby", 32), ("focus", 12), ("townhall", 200)] {
            let room = Room::new(RoomId::new(room_id)?, capacity)?;
            repository.save(room);
        }
        Ok(repository)
    }
}

impl RoomRepository for InMemoryRoomRepository {
    fn save(&mut self, room: Room) {
        self.rooms.insert(room.id().clone(), room);
    }

    fn get(&self, id: &RoomId) -> Option<Room> {
        self.rooms.get(id).cloned()
    }

    fn list(&self) -> Vec<Room> {
        self.rooms.values().cloned().collect()
    }
}
