use crate::common::{SharedFSState, FSEvent, ObjectType};
use std::path::Path;
use napi::bindgen_prelude::*;
use windows::Win32::Storage::ProjectedFileSystem::*;
use windows::Win32::Foundation::*;
use std::sync::Arc;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

pub struct FSImpl {
	instance_handle: Option<HPRJFS>,
	state: SharedFSState,
}

impl FSImpl {
	pub fn new(state: SharedFSState) -> Self {
		Self { 
			instance_handle: None,
			state,
		}
	}

	pub async fn mount(&mut self, mount_path: &Path) -> Result<()> {
		unsafe {
			let root_path = mount_path.to_str().unwrap();
			let mut instance_handle = HPRJFS::default();
			
			let state = self.state.clone();
			let notification_cb = PRJ_NOTIFICATION_CB(Some(notification_callback));
			
			let callbacks = PRJ_CALLBACKS {
				StartDirectoryEnumerationCallback: PRJ_START_DIRECTORY_ENUMERATION_CB(Some(start_dir_enum)),
				EndDirectoryEnumerationCallback: PRJ_END_DIRECTORY_ENUMERATION_CB(Some(end_dir_enum)),
				GetDirectoryEnumerationCallback: PRJ_GET_DIRECTORY_ENUMERATION_CB(Some(get_dir_enum)),
				GetPlaceholderInfoCallback: PRJ_GET_PLACEHOLDER_INFO_CB(Some(get_placeholder_info)),
				GetFileDataCallback: PRJ_GET_FILE_DATA_CB(Some(get_file_data)),
				NotificationCallback: notification_cb,
				..Default::default()
			};

			let options = PRJ_STARTVIRTUALIZING_OPTIONS {
				Flags: PRJ_STARTVIRTUALIZING_FLAGS(0),
				PoolThreadCount: 0,
				ConcurrentThreadCount: 0,
				NotificationMappings: std::ptr::null(),
				NotificationMappingsCount: 0,
			};

			PrjStartVirtualizing(
				PCWSTR::from_raw(root_path.encode_utf16().collect::<Vec<_>>().as_ptr()),
				&callbacks,
				std::ptr::null(),
				&options as *const _,
				&mut instance_handle,
			)?;

			self.instance_handle = Some(instance_handle);
			Ok(())
		}
	}

	pub async fn unmount(&mut self) -> Result<()> {
		if let Some(handle) = self.instance_handle.take() {
			unsafe {
				PrjStopVirtualizing(handle)?;
			}
		}
		Ok(())
	}
}

extern "system" fn notification_callback(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	notification_data: *const PRJ_NOTIFICATION_DATA,
	is_directory: bool,
) -> HRESULT {
	unsafe {
		let notification = &*notification_data;
		let file_path = get_string_from_pcwstr(notification.NotificationRoot);
		let object_type = if is_directory { ObjectType::Directory } else { ObjectType::File };
		
		// Get the tokio runtime
		if let Ok(rt) = tokio::runtime::Runtime::new() {
			rt.block_on(async move {
				if let Some(state) = get_state() {
					let mut state = state.write().await;
					match notification.NotificationMask.0 {
						PRJ_NOTIFICATION_FILE_OPENED => {
							// File opened, no event needed
						}
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
	}
	S_OK
}

// Helper function to convert Windows wide string to Rust String
fn get_string_from_pcwstr(pcwstr: PCWSTR) -> String {
	unsafe {
		let len = (0..).take_while(|&i| *pcwstr.0.add(i) != 0).count();
		let slice = std::slice::from_raw_parts(pcwstr.0, len);
		OsString::from_wide(slice).to_string_lossy().into_owned()
	}
}

// Keep existing callback implementations...
extern "system" fn start_dir_enum(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	_enumeration_id: GUID,
) -> HRESULT {
	S_OK
}

extern "system" fn end_dir_enum(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	_enumeration_id: GUID,
) -> HRESULT {
	S_OK
}

extern "system" fn get_dir_enum(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	_enumeration_id: GUID,
	_search_expression: PCWSTR,
	_dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
	S_OK
}

extern "system" fn get_placeholder_info(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	_file_path: PCWSTR,
	_trigger_info: *const PRJ_PLACEHOLDER_INFO,
) -> HRESULT {
	S_OK
}

extern "system" fn get_file_data(
	_namespace_virtualization_context: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
	_file_path: PCWSTR,
	_byte_offset: u64,
	_length: u32,
) -> HRESULT {
	S_OK
}

// Thread-local storage for the state
thread_local! {
	static THREAD_STATE: std::cell::RefCell<Option<SharedFSState>> = std::cell::RefCell::new(None);
}

fn get_state() -> Option<SharedFSState> {
	THREAD_STATE.with(|state| state.borrow().clone())
} 