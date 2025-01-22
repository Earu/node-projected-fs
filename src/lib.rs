#![deny(clippy::all)]

use napi::bindgen_prelude::*;
use napi_derive::napi;
use napi::threadsafe_function::ThreadsafeFunction;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod common;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use common::{SharedFSState, create_fs_state, FSEvent};
#[cfg(unix)]
use unix::FSImpl;
#[cfg(windows)]
use windows::FSImpl;

#[napi(object)]
pub struct FileSystemEvent {
	pub event_type: String,
	pub path: String,
	pub object_type: String,
}

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
		state.files.insert(path.clone(), common::VirtualFile {
			content: content.to_vec(),
			size: content.len() as u64,
			is_directory: false,
		});
		state.emit_event(FSEvent::Created { path, object_type: common::ObjectType::File });
		Ok(())
	}

	#[napi]
	pub async fn add_directory(&self, path: String) -> Result<()> {
		let mut state = self.state.write().await;
		state.files.insert(path.clone(), common::VirtualFile {
			content: vec![],
			size: 0,
			is_directory: true,
		});
		state.emit_event(FSEvent::Created { path, object_type: common::ObjectType::Directory });
		Ok(())
	}

	#[napi]
	pub async fn remove_path(&self, path: String) -> Result<()> {
		let mut state = self.state.write().await;
		if let Some(file) = state.files.remove(&path) {
			state.emit_event(FSEvent::Deleted { path, object_type: file.get_type() });
		}
		Ok(())
	}

	#[napi(js_name = "on")]
	pub fn on_fs_event(&self, callback: JsFunction) -> Result<()> {
		let state = self.state.clone();
		let tsfn: ThreadsafeFunction<_, napi::threadsafe_function::ErrorStrategy::Fatal> = 
			callback.create_threadsafe_function(0, |ctx| {
				let event = ctx.value;
				Ok(vec![event])
			})?;

		std::thread::spawn(move || {
			let rt = tokio::runtime::Runtime::new().unwrap();
			rt.block_on(async move {
				let state = state.read().await;
				let mut rx = state.subscribe_to_events();
				drop(state);

				while let Ok(event) = rx.recv().await {
					let (event_type, path, object_type) = match event {
						FSEvent::Created { path, object_type } => ("created", path, object_type),
						FSEvent::Modified { path, object_type } => ("modified", path, object_type),
						FSEvent::Deleted { path, object_type } => ("deleted", path, object_type),
					};

					let js_event = FileSystemEvent {
						event_type: event_type.to_string(),
						path,
						object_type: match object_type {
							common::ObjectType::File => "file".to_string(),
							common::ObjectType::Directory => "directory".to_string(),
						},
					};

					let _ = tsfn.call(js_event, napi::threadsafe_function::ThreadsafeFunctionCallMode::Blocking);
				}
			});
		});

		Ok(())
	}
} 