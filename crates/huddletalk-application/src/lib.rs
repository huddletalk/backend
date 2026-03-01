use huddletalk_domain::{DomainError, Room, RoomId, RoomRepository};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRoomCommand {
    pub room_id: String,
    pub capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    pub room_id: String,
    pub capacity: usize,
}

pub struct RoomApplicationService<R: RoomRepository> {
    room_repository: R,
}

impl<R: RoomRepository> RoomApplicationService<R> {
    pub fn new(room_repository: R) -> Self {
        Self { room_repository }
    }

    pub fn create_room(&mut self, command: CreateRoomCommand) -> Result<RoomView, DomainError> {
        let room_id = RoomId::new(command.room_id)?;
        let room = Room::new(room_id.clone(), command.capacity)?;
        let room_view = RoomView {
            room_id: room_id.as_str().to_owned(),
            capacity: command.capacity,
        };

        self.room_repository.save(room);
        Ok(room_view)
    }

    pub fn list_rooms(&self) -> Vec<RoomView> {
        let mut rooms = self
            .room_repository
            .list()
            .into_iter()
            .map(|room| RoomView {
                room_id: room.id().as_str().to_owned(),
                capacity: room.capacity(),
            })
            .collect::<Vec<_>>();

        rooms.sort_by(|left, right| left.room_id.cmp(&right.room_id));
        rooms
    }
}
