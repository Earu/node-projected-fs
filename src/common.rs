use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

#[derive(Clone, Debug)]
pub enum ObjectType {
	File,
	Directory,
}

#[derive(Clone, Debug)]
pub enum FSEvent {
	Created { path: String, object_type: ObjectType },
	Modified { path: String, object_type: ObjectType },
	Deleted { path: String, object_type: ObjectType },
}

#[derive(Default)]
pub struct VirtualFile {
	pub content: Vec<u8>,
	pub size: u64,
	pub is_directory: bool,
}

impl VirtualFile {
	pub fn get_type(&self) -> ObjectType {
		if self.is_directory {
			ObjectType::Directory
		} else {
			ObjectType::File
		}
	}
}

pub struct FSState {
	pub files: HashMap<String, VirtualFile>,
	event_sender: broadcast::Sender<FSEvent>,
}

impl Default for FSState {
	fn default() -> Self {
		let (event_sender, _) = broadcast::channel(100);
		Self {
			files: HashMap::new(),
			event_sender,
		}
	}
}

impl FSState {
	pub fn emit_event(&self, event: FSEvent) {
		let _ = self.event_sender.send(event);
	}

	pub fn subscribe_to_events(&self) -> broadcast::Receiver<FSEvent> {
		self.event_sender.subscribe()
	}
}

pub type SharedFSState = Arc<RwLock<FSState>>;

pub fn create_fs_state() -> SharedFSState {
	Arc::new(RwLock::new(FSState::default()))
} 