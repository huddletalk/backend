use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    EmptyRoomId,
    InvalidCapacity(usize),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoomId => write!(f, "room id cannot be empty"),
            Self::InvalidCapacity(capacity) => {
                write!(f, "room capacity must be greater than 0, got {capacity}")
            }
        }
    }
}

impl Error for DomainError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoomId(String);

impl RoomId {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let normalized = value.trim();
        if normalized.is_empty() {
            return Err(DomainError::EmptyRoomId);
        }

        Ok(Self(normalized.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    id: RoomId,
    capacity: usize,
}

impl Room {
    pub fn new(id: RoomId, capacity: usize) -> Result<Self, DomainError> {
        if capacity == 0 {
            return Err(DomainError::InvalidCapacity(capacity));
        }

        Ok(Self { id, capacity })
    }

    pub fn id(&self) -> &RoomId {
        &self.id
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

pub trait RoomRepository {
    fn save(&mut self, room: Room);
    fn get(&self, id: &RoomId) -> Option<Room>;
    fn list(&self) -> Vec<Room>;
}
