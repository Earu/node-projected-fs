use crate::common::{SharedFSState, FSEvent, ObjectType};
use std::path::Path;
use napi::bindgen_prelude::*;
use windows::Win32::Storage::ProjectedFileSystem::*;
use windows::Win32::Foundation::*;
use windows::core::{PCWSTR, HRESULT, GUID};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::Mutex;
use std::collections::HashMap;
use once_cell::sync::Lazy;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

// Define a static GUID for our provider
static PROVIDER_GUID: GUID = GUID::from_values(
	0x2e7dacb1,
	0x8a87,
	0x4c61,
	[0x93, 0x2c, 0x47, 0x90, 0x60, 0x5a, 0xb7, 0x9f],
);

// Global state mapping using the raw pointer value as the key
static INSTANCE_STATES: Lazy<Mutex<HashMap<usize, SharedFSState>>> =
	Lazy::new(|| Mutex::new(HashMap::new()));

pub struct FSImpl {
	session: Option<VirtualFS>,
	state: SharedFSState,
	pub total_space_bytes: u64,
	pub max_files: u64,
}

impl FSImpl {
	pub fn new(state: SharedFSState) -> Self {
		// Default to 4GB total space and 1M files like Unix
		Self::with_size(state, 4 * 1024 * 1024 * 1024, 1024 * 1024)
	}

	pub fn with_size(state: SharedFSState, total_space_bytes: u64, max_files: u64) -> Self {
		Self {
			session: None,
			state,
			total_space_bytes,
			max_files,
		}
	}

	pub async fn mount(&mut self, mount_path: &Path) -> Result<()> {
		let mut fs = VirtualFS::new(
			self.state.clone(),
			self.total_space_bytes,
			self.max_files,
		);

		match fs.start(mount_path) {
			Ok(()) => {
				self.session = Some(fs);
				Ok(())
			},
			Err(e) => Err(Error::from_reason(format!("Mount failed: {:?}", e)))
		}
	}

	pub async fn unmount(&mut self) -> Result<()> {
		if let Some(mut fs) = self.session.take() {
			fs.stop();
		}
		Ok(())
	}
}

struct VirtualFS {
	state: SharedFSState,
	total_space_bytes: u64,
	max_files: u64,
	instance_handle: Option<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT>,
}

impl VirtualFS {
	fn new(state: SharedFSState, total_space_bytes: u64, max_files: u64) -> Self {
		Self {
			state,
			total_space_bytes,
			max_files,
			instance_handle: None,
		}
	}

	fn start(&mut self, mount_path: &Path) -> windows::core::Result<()> {
		unsafe {
			let root_path = mount_path.to_str().unwrap();
			let instance_handle = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();

			// Convert path to wide string and ensure it stays alive
			let root_path_wide: Vec<u16> = root_path.encode_utf16().chain(std::iter::once(0)).collect();

			// Mark directory as a reparse point for ProjFS
			let version_info = PRJ_PLACEHOLDER_VERSION_INFO {
				ProviderID: [0; 128],  // Using zeros for now
				ContentID: [0; 128],   // Using zeros for now
			};

			let result = PrjMarkDirectoryAsPlaceholder(
				PCWSTR(root_path_wide.as_ptr()),
				None,  // No target path needed
				Some(&version_info),
				&PROVIDER_GUID,
			);

			if let Err(e) = result {
				return Err(e);
			}

			let callbacks = PRJ_CALLBACKS {
				StartDirectoryEnumerationCallback: Some(Self::start_dir_enum),
				EndDirectoryEnumerationCallback: Some(Self::end_dir_enum),
				GetDirectoryEnumerationCallback: Some(Self::get_dir_enum),
				GetPlaceholderInfoCallback: Some(Self::get_placeholder_info),
				GetFileDataCallback: Some(Self::get_file_data),
				NotificationCallback: Some(Self::notification_callback),
				..Default::default()
			};

			let options = PRJ_STARTVIRTUALIZING_OPTIONS {
				Flags: PRJ_STARTVIRTUALIZING_FLAGS(0),
				PoolThreadCount: 0,
				ConcurrentThreadCount: 0,
				NotificationMappings: std::ptr::null_mut(),
				NotificationMappingsCount: 0,
			};

			let result = PrjStartVirtualizing(
				PCWSTR(root_path_wide.as_ptr()),
				&callbacks,
				Some(&instance_handle as *const _ as *const std::ffi::c_void),
				Some(&options),
			);

			if result.is_ok() {
				// Store the state in the global map using the handle's pointer value as key
				if let Ok(mut states) = INSTANCE_STATES.lock() {
					let key = instance_handle.0 as usize;
					states.insert(key, self.state.clone());
				}
				self.instance_handle = Some(instance_handle);
			}

			result.map(|_| ())
		}
	}

	fn stop(&mut self) {
		if let Some(handle) = self.instance_handle.take() {
			unsafe {
				// Remove the state from the global map
				if let Ok(mut states) = INSTANCE_STATES.lock() {
					let key = handle.0 as usize;
					states.remove(&key);
				}
				PrjStopVirtualizing(handle);
			}
		}
	}

	unsafe extern "system" fn notification_callback(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_is_directory: BOOLEAN,
		_notification: PRJ_NOTIFICATION,
		_destination_file_name: PCWSTR,
		_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
	) -> HRESULT {
		// Get the tokio runtime
		if let Ok(rt) = tokio::runtime::Runtime::new() {
			rt.block_on(async move {
				let state = Self::get_state_from_context(_callback_data);
				if let Some(state) = state {
					let state = state.write().await;
					let object_type = if _is_directory.as_bool() { ObjectType::Directory } else { ObjectType::File };
					let file_path = Self::get_string_from_pcwstr(_destination_file_name);

					match _notification {
						PRJ_NOTIFICATION_NEW_FILE_CREATED => {
							state.emit_event(FSEvent::Created { path: file_path, object_type });
						}
						PRJ_NOTIFICATION_FILE_OVERWRITTEN | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => {
							state.emit_event(FSEvent::Modified { path: file_path, object_type });
						}
						PRJ_NOTIFICATION_FILE_DELETED => {
							state.emit_event(FSEvent::Deleted { path: file_path, object_type });
						}
						_ => {}
					}
				}
			});
		}
		HRESULT(0)
	}

	unsafe extern "system" fn start_dir_enum(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_enumeration_id: *const GUID,
	) -> HRESULT {
		HRESULT(0)
	}

	unsafe extern "system" fn end_dir_enum(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_enumeration_id: *const GUID,
	) -> HRESULT {
		HRESULT(0)
	}

	unsafe extern "system" fn get_dir_enum(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_enumeration_id: *const GUID,
		_search_expression: PCWSTR,
		dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
	) -> HRESULT {
		if let Ok(rt) = tokio::runtime::Runtime::new() {
			return rt.block_on(async move {
				let state = Self::get_state_from_context(_callback_data);
				if let Some(state) = state {
					let state = state.read().await;
					let parent_path = Self::get_string_from_pcwstr((*_callback_data).FilePathName);

					for (path, file) in state.files.iter() {
						let is_direct_child = if parent_path.is_empty() {
							!path.contains('/')
						} else {
							path.starts_with(&format!("{}/", parent_path)) &&
							path[parent_path.len()+1..].split('/').count() == 1
						};

						if is_direct_child {
							let name = path.split('/').last().unwrap();
							let name_wide: Vec<u16> = name.encode_utf16().collect();

							let file_info = PRJ_FILE_BASIC_INFO {
								IsDirectory: BOOLEAN::from(file.is_directory),
								FileSize: file.size as i64,
								CreationTime: 0,
								LastAccessTime: 0,
								LastWriteTime: 0,
								ChangeTime: 0,
								FileAttributes: if file.is_directory {
									FILE_ATTRIBUTE_DIRECTORY
								} else {
									FILE_ATTRIBUTE_NORMAL
								},
								..Default::default()
							};

							let name_ptr = name_wide.as_ptr();
							if PrjFillDirEntryBuffer(
								PCWSTR(name_ptr),
								Some(&file_info),
								dir_entry_buffer_handle,
							).is_err() {
								return HRESULT(-2147024896); // E_FAIL
							}
						}
					}
				}
				HRESULT(0)
			});
		}
		HRESULT(0)
	}

	unsafe extern "system" fn get_placeholder_info(
		_callback_data: *const PRJ_CALLBACK_DATA,
	) -> HRESULT {
		if let Ok(rt) = tokio::runtime::Runtime::new() {
			return rt.block_on(async move {
				let state = Self::get_state_from_context(_callback_data);
				if let Some(state) = state {
					let state = state.read().await;
					let path = Self::get_string_from_pcwstr((*_callback_data).FilePathName);

					if let Some(file) = state.files.get(&path) {
						let placeholder_info = PRJ_PLACEHOLDER_INFO {
							FileBasicInfo: PRJ_FILE_BASIC_INFO {
								IsDirectory: BOOLEAN::from(file.is_directory),
								FileSize: file.size as i64,
								CreationTime: 0,
								LastAccessTime: 0,
								LastWriteTime: 0,
								ChangeTime: 0,
								FileAttributes: if file.is_directory {
									FILE_ATTRIBUTE_DIRECTORY
								} else {
									FILE_ATTRIBUTE_NORMAL
								},
								..Default::default()
							},
							..Default::default()
						};

						if PrjWritePlaceholderInfo(
							(*_callback_data).NamespaceVirtualizationContext,
							(*_callback_data).FilePathName,
							&placeholder_info,
							std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
						).is_err() {
							return HRESULT(-2147024896); // E_FAIL
						}
					}
				}
				HRESULT(0)
			});
		}
		HRESULT(0)
	}

	unsafe extern "system" fn get_file_data(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_byte_offset: u64,
		_length: u32,
	) -> HRESULT {
		if let Ok(rt) = tokio::runtime::Runtime::new() {
			return rt.block_on(async move {
				let state = Self::get_state_from_context(_callback_data);
				if let Some(state) = state {
					let state = state.read().await;
					let path = Self::get_string_from_pcwstr((*_callback_data).FilePathName);

					if let Some(file) = state.files.get(&path) {
						let start = _byte_offset as usize;
						let end = std::cmp::min(start + _length as usize, file.content.len());

						if start < file.content.len() {
							let data = &file.content[start..end];
							let result = PrjWriteFileData(
								(*_callback_data).NamespaceVirtualizationContext,
								(*_callback_data).FilePathName.0 as *const _,
								data.as_ptr() as *const _,
								_byte_offset,
								data.len() as u32,
							);
							if result.is_err() {
								return HRESULT(-2147024896); // E_FAIL
							}
						}
					}
				}
				HRESULT(0)
			});
		}
		HRESULT(0)
	}

	// Helper function to convert Windows wide string to Rust String
	fn get_string_from_pcwstr(pcwstr: PCWSTR) -> String {
		unsafe {
			let len = (0..).take_while(|&i| *pcwstr.0.add(i) != 0).count();
			let slice = std::slice::from_raw_parts(pcwstr.0, len);
			OsString::from_wide(slice).to_string_lossy().into_owned()
		}
	}

	// Helper function to get state from callback context
	fn get_state_from_context(callback_data: *const PRJ_CALLBACK_DATA) -> Option<SharedFSState> {
		unsafe {
			let instance_handle = (*callback_data).NamespaceVirtualizationContext;
			let key = instance_handle.0 as usize;
			if let Ok(states) = INSTANCE_STATES.lock() {
				states.get(&key).cloned()
			} else {
				None
			}
		}
	}
}