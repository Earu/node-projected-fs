use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Default)]
pub struct VirtualFile {
	pub content: Vec<u8>,
	pub size: u64,
	pub is_directory: bool,
}

#[derive(Default)]
pub struct FSState {
	pub files: HashMap<String, VirtualFile>,
}

pub type SharedFSState = Arc<RwLock<FSState>>;

pub fn create_fs_state() -> SharedFSState {
	Arc::new(RwLock::new(FSState::default()))
} 