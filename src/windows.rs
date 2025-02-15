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
use std::time::SystemTime;
use uuid::Uuid;
use std::path::PathBuf;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

// Global state mapping using the raw pointer value as the key
static INSTANCE_STATES: Lazy<Mutex<HashMap<usize, SharedFSState>>> =
	Lazy::new(|| Mutex::new(HashMap::new()));

// Add this near the top with other statics
static ENUM_STATES: Lazy<Mutex<HashMap<String, usize>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub struct FSImpl {
	sessions: HashMap<PathBuf, VirtualFS>,
	state: SharedFSState,
	pub total_space_bytes: u64,
	pub max_files: u64,
	provider_guid: GUID,
}

impl FSImpl {
	pub fn new(state: SharedFSState) -> Self {
		Self::with_size(state, 4 * 1024 * 1024 * 1024, 1024 * 1024)
	}

	pub fn with_size(
		state: SharedFSState,
		total_space_bytes: u64,
		max_files: u64,
	) -> Self {
		// Generate a random UUID and convert it to Windows GUID
		let uuid = Uuid::new_v4();
		let provider_guid = GUID::from_values(
			uuid.as_fields().0,
			uuid.as_fields().1,
			uuid.as_fields().2,
			uuid.as_fields().3.clone(),
		);

		Self {
			sessions: HashMap::new(),
			state,
			total_space_bytes,
			max_files,
			provider_guid,
		}
	}

	pub async fn mount(&mut self, mount_path: &Path) -> Result<()> {
		let mut fs = VirtualFS::new(
			self.state.clone(),
			self.total_space_bytes,
			self.max_files,
			self.provider_guid,
		);

		match fs.start(mount_path) {
			Ok(()) => {
				self.sessions.insert(mount_path.to_path_buf(), fs);
				Ok(())
			},
			Err(e) => Err(Error::from_reason(format!("Mount failed: {:?}", e)))
		}
	}

	pub async fn unmount(&mut self, mount_path: &Path) -> Result<()> {
		if let Some(mut fs) = self.sessions.remove(mount_path) {
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
	provider_guid: GUID,
}

impl VirtualFS {
	fn new(
		state: SharedFSState,
		total_space_bytes: u64,
		max_files: u64,
		provider_guid: GUID,
	) -> Self {
		Self {
			state,
			total_space_bytes,
			max_files,
			instance_handle: None,
			provider_guid,
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
				None,
				Some(&version_info),
				&self.provider_guid,
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

			// Store state in global map before starting virtualization
			let state_ptr = Box::into_raw(Box::new(self.state.clone())) as *const std::ffi::c_void;
			if let Ok(mut states) = INSTANCE_STATES.lock() {
				let key = state_ptr as usize;
				states.insert(key, self.state.clone());
			}

			let result = PrjStartVirtualizing(
				PCWSTR(root_path_wide.as_ptr()),
				&callbacks,
				Some(state_ptr),
				Some(&options),
			);

			if let Err(_) = &result {
				// Clean up on error
				if let Ok(mut states) = INSTANCE_STATES.lock() {
					states.remove(&(state_ptr as usize));
				}
			} else {
				self.instance_handle = Some(instance_handle);
			}

			result.map(|_| ())
		}
	}

	fn stop(&mut self) {
		if let Some(handle) = self.instance_handle.take() {
			unsafe {
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

					// Only emit deletion events for explicit file deletions
					// Ignore notifications that might be from internal ProjFS operations
					match _notification {
						PRJ_NOTIFICATION_NEW_FILE_CREATED => {
							state.emit_event(FSEvent::Created { path: file_path, object_type });
						}
						PRJ_NOTIFICATION_FILE_OVERWRITTEN | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => {
							state.emit_event(FSEvent::Modified { path: file_path, object_type });
						}
						PRJ_NOTIFICATION_PRE_DELETE => {
							// Only emit deletion if the file was actually in our state
							let lookup_path = file_path.replace('\\', "/");
							if state.files.contains_key(&lookup_path) {
								state.emit_event(FSEvent::Deleted { path: file_path, object_type });
							}
						},
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
		// Initialize enumeration state
		let guid_str = format!("{:?}", unsafe { *_enumeration_id });
		if let Ok(mut states) = ENUM_STATES.lock() {
			states.insert(guid_str, 0);
		}
		HRESULT(0)
	}

	unsafe extern "system" fn end_dir_enum(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_enumeration_id: *const GUID,
	) -> HRESULT {
		// Clean up enumeration state
		let guid_str = format!("{:?}", unsafe { *_enumeration_id });
		if let Ok(mut states) = ENUM_STATES.lock() {
			states.remove(&guid_str);
		}
		HRESULT(0)
	}

	unsafe extern "system" fn get_dir_enum(
		_callback_data: *const PRJ_CALLBACK_DATA,
		_enumeration_id: *const GUID,
		_search_expression: PCWSTR,
		dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
	) -> HRESULT {
		let guid_str = format!("{:?}", unsafe { *_enumeration_id });

		if let Ok(rt) = tokio::runtime::Runtime::new() {
			return rt.block_on(async move {
				let state = Self::get_state_from_context(_callback_data);
				if let Some(state) = state {
					let state = state.read().await;
					let parent_path = Self::get_string_from_pcwstr((*_callback_data).FilePathName).replace('\\', "/");

					// Get current index for this enumeration
					let mut current_index = 0;
					if let Ok(states) = ENUM_STATES.lock() {
						current_index = *states.get(&guid_str).unwrap_or(&0);
					}

					// First collect all direct children
					let mut children = Vec::new();
					for (path, file) in state.files.iter() {
						let is_direct_child = if parent_path.is_empty() {
							!path.contains('/')
						} else {
							path.starts_with(&format!("{}/", parent_path)) &&
							path[parent_path.len()+1..].split('/').count() == 1
						};

						if is_direct_child {
							let name = path.split('/').last().unwrap();
							children.push((name.to_string(), file));
						}
					}

					// If we've sent all entries, clean up and return STATUS_END_OF_FILE
					if current_index >= children.len() {
						if let Ok(mut states) = ENUM_STATES.lock() {
							states.remove(&guid_str);
						}
						return HRESULT(-2147483633); // STATUS_END_OF_FILE
					}

					// Add the next child to the buffer
					let (name, file) = &children[current_index];
					let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

					let file_info = PRJ_FILE_BASIC_INFO {
						IsDirectory: BOOLEAN::from(file.is_directory),
						FileSize: file.size as i64,
						CreationTime: Self::system_time_to_file_time(file.mtime),
						LastAccessTime: Self::system_time_to_file_time(file.mtime),
						LastWriteTime: Self::system_time_to_file_time(file.mtime),
						ChangeTime: Self::system_time_to_file_time(file.mtime),
						FileAttributes: if file.is_directory {
							FILE_ATTRIBUTE_DIRECTORY
						} else {
							FILE_ATTRIBUTE_NORMAL
						},
						..Default::default()
					};

					let result = PrjFillDirEntryBuffer(
						PCWSTR(name_wide.as_ptr()),
						Some(&file_info),
						dir_entry_buffer_handle,
					);

					if result.is_err() {
						return HRESULT(-2147024896); // E_FAIL
					}

					// Update the index for next time
					if let Ok(mut states) = ENUM_STATES.lock() {
						states.insert(guid_str, current_index + 1);
					}

					HRESULT(0)
				} else {
					HRESULT(-2147483633) // STATUS_END_OF_FILE
				}
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
					let path = Self::get_string_from_pcwstr((*_callback_data).FilePathName).replace('\\', "/");

					if let Some(file) = state.files.get(&path) {
						let placeholder_info = PRJ_PLACEHOLDER_INFO {
							FileBasicInfo: PRJ_FILE_BASIC_INFO {
								IsDirectory: BOOLEAN::from(file.is_directory),
								FileSize: file.size as i64,
								CreationTime: Self::system_time_to_file_time(file.mtime),
								LastAccessTime: Self::system_time_to_file_time(file.mtime),
								LastWriteTime: Self::system_time_to_file_time(file.mtime),
								ChangeTime: Self::system_time_to_file_time(file.mtime),
								FileAttributes: if file.is_directory {
									FILE_ATTRIBUTE_DIRECTORY
								} else {
									FILE_ATTRIBUTE_NORMAL
								},
								..Default::default()
							},
							VariableData: [0; 1],
							EaInformation: Default::default(),
							SecurityInformation: Default::default(),
							StreamsInformation: Default::default(),
							VersionInfo: PRJ_PLACEHOLDER_VERSION_INFO {
								ProviderID: [0; 128],
								ContentID: [0; 128],
							},
						};

						if PrjWritePlaceholderInfo(
							(*_callback_data).NamespaceVirtualizationContext,
							(*_callback_data).FilePathName,
							&placeholder_info,
							std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
						).is_err() {
							return HRESULT(-2147024896); // E_FAIL
						}
						return HRESULT(0);
					}
					return HRESULT(-2147024894); // E_FILE_NOT_FOUND
				}
				HRESULT(-2147024894) // E_FILE_NOT_FOUND
			});
		}
		HRESULT(-2147024894) // E_FILE_NOT_FOUND
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
					let path = Self::get_string_from_pcwstr((*_callback_data).FilePathName).replace('\\', "/");

					if let Some(file) = state.files.get(&path) {
						let start = _byte_offset as usize;
						let end = std::cmp::min(start + _length as usize, file.content.len());

						if start < file.content.len() {
							let data = &file.content[start..end];
							let result = PrjWriteFileData(
								(*_callback_data).NamespaceVirtualizationContext,
								&(*_callback_data).DataStreamId,
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
			let context_ptr = (*callback_data).InstanceContext;
			if !context_ptr.is_null() {
				// Instead of dereferencing and cloning, use the global map with the context pointer as key
				if let Ok(states) = INSTANCE_STATES.lock() {
					let key = context_ptr as usize;
					states.get(&key).cloned()
				} else {
					None
				}
			} else {
				None
			}
		}
	}

	fn system_time_to_file_time(time: SystemTime) -> i64 {
		// Windows FILETIME is in 100-nanosecond intervals since January 1, 1601 UTC
		// First convert to duration since Unix epoch
		let duration = time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();

		// Convert Unix timestamp to Windows timestamp
		// Add number of 100-nanosecond intervals between 1601 and 1970
		const WINDOWS_UNIX_EPOCH_DIFF: i64 = 116444736000000000;
		(duration.as_nanos() as i64 / 100) + WINDOWS_UNIX_EPOCH_DIFF
	}
}