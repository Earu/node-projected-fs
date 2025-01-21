#![deny(clippy::all)]

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod common;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use common::{SharedFSState, create_fs_state};
#[cfg(unix)]
use unix::FSImpl;
#[cfg(windows)]
use windows::FSImpl;

#[napi(js_name = "FuseFS")]
pub struct JsFuseFS {
	inner: Arc<Mutex<FSImpl>>,
	state: SharedFSState,
	mount_path: Arc<Mutex<Option<PathBuf>>>,
	unmount_sender: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

#[napi]
impl JsFuseFS {
	#[napi(constructor)]
	pub fn new() -> Self {
		let state = create_fs_state();
		JsFuseFS {
			inner: Arc::new(Mutex::new(FSImpl::new(state.clone()))),
			state,
			mount_path: Arc::new(Mutex::new(None)),
			unmount_sender: Arc::new(Mutex::new(None)),
		}
	}

	#[napi]
	pub async fn mount(&self, path: String) -> Result<()> {
		let mount_path = PathBuf::from(path);
		*self.mount_path.lock().await = Some(mount_path.clone());

		let (tx, rx) = tokio::sync::oneshot::channel();
		*self.unmount_sender.lock().await = Some(tx);

		let inner = self.inner.clone();
		
		std::thread::spawn(move || {
			let rt = tokio::runtime::Runtime::new().unwrap();
			rt.block_on(async {
				inner.lock().await.mount(&mount_path).await?;
				rx.await.ok();
				inner.lock().await.unmount().await
			}).unwrap_or_else(|e| eprintln!("Mount error: {}", e));
		});

		Ok(())
	}

	#[napi]
	pub async fn unmount(&self) -> Result<()> {
		if let Some(sender) = self.unmount_sender.lock().await.take() {
			sender.send(()).ok();
		}
		Ok(())
	}

	#[napi]
	pub async fn add_file(&self, path: String, content: Buffer) -> Result<()> {
		let mut state = self.state.write().await;
		state.files.insert(path, common::VirtualFile {
			content: content.to_vec(),
			size: content.len() as u64,
			is_directory: false,
		});
		Ok(())
	}

	#[napi]
	pub async fn add_directory(&self, path: String) -> Result<()> {
		let mut state = self.state.write().await;
		state.files.insert(path, common::VirtualFile {
			content: vec![],
			size: 0,
			is_directory: true,
		});
		Ok(())
	}

	#[napi]
	pub async fn remove_path(&self, path: String) -> Result<()> {
		let mut state = self.state.write().await;
		state.files.remove(&path);
		Ok(())
	}
} 