use crate::common::{SharedFSState, FSEvent, ObjectType};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH, SystemTime};
use fuser::{
	FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
	Request, ReplyWrite, ReplyCreate, TimeOrNow,
};
use napi::bindgen_prelude::*;

const TTL: Duration = Duration::from_secs(1);

// Get current user's UID and GID
fn get_user_ids() -> (u32, u32) {
    #[cfg(unix)]
    {
        (unsafe { libc::getuid() }, unsafe { libc::getgid() })
    }
    #[cfg(not(unix))]
    {
        (1000, 1000)
    }
}

pub struct FSImpl {
	session: Option<fuser::BackgroundSession>,
	state: SharedFSState,
	pub total_space_bytes: u64,
	pub max_files: u64,
}

impl FSImpl {
	pub fn new(state: SharedFSState) -> Self {
		// Default to 4GB total space and 1M files
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
		let options = vec![
			MountOption::FSName("virtual".to_string()),
			MountOption::DefaultPermissions,
			MountOption::AutoUnmount,
		];
		
		let fs = VirtualFS {
			state: self.state.clone(),
			total_space_bytes: self.total_space_bytes,
			max_files: self.max_files,
		};
		
		match fuser::spawn_mount2(fs, mount_path, &options) {
			Ok(session) => {
				self.session = Some(session);
				Ok(())
			},
			Err(e) => {
				eprintln!("FUSE mount error details: {:?}", e);
				Err(Error::from_reason(format!("Mount failed: {:?}", e)))
			}
		}
	}

	pub async fn unmount(&mut self) -> Result<()> {
		self.session.take();
		Ok(())
	}
}

struct VirtualFS {
	state: SharedFSState,
	total_space_bytes: u64,
	max_files: u64,
}

impl Filesystem for VirtualFS {
	fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			let (uid, gid) = get_user_ids();
			
			let parent_path = if parent == 1 {
				String::new()
			} else {
				let parent_path = state.files.iter()
					.find(|(path, file)| file.is_directory && hash_path(path) == parent)
					.map(|(path, _)| path.clone());
				
				match parent_path {
					Some(path) => path,
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			if let Some(file) = state.files.get(&path) {
				let attr = FileAttr {
					ino: hash_path(&path),
					size: file.size,
					blocks: 1,
					atime: UNIX_EPOCH,
					mtime: UNIX_EPOCH,
					ctime: UNIX_EPOCH,
					crtime: UNIX_EPOCH,
					kind: if file.is_directory { FileType::Directory } else { FileType::RegularFile },
					perm: if file.is_directory { 0o755 } else { 0o644 },
					nlink: if file.is_directory { 2 } else { 1 },
					uid,
					gid,
					rdev: 0,
					flags: 0,
					blksize: 512,
				};
				reply.entry(&TTL, &attr, 0);
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn write(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyWrite) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			let now = SystemTime::now();
			
			let mut found_path = None;
			let mut is_dir = false;
			
			// Calculate current total size
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();
			
			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					found_path = Some(path.clone());
					break;
				}
			}

			if let Some(path) = found_path {
				if let Some(file) = state.files.get_mut(&path) {
					let start = offset as usize;
					let end = start + data.len();
					
					// Calculate the size change
					let size_increase = if end > file.content.len() {
						(end - file.content.len()) as u64
					} else {
						0
					};

					// Check if this write would exceed the total space limit
					if total_size + size_increase > self.total_space_bytes {
						reply.error(libc::ENOSPC);
						return;
					}
					
					// Ensure the file is large enough
					if end > file.content.len() {
						file.content.resize(end, 0);
					}
					
					// Write the data
					file.content[start..end].copy_from_slice(data);
					file.size = file.content.len() as u64;
					file.mtime = now;
					is_dir = file.is_directory;
				}

				// Emit modification event outside the mutable borrow scope
				state.emit_event(FSEvent::Modified { 
					path,
					object_type: if is_dir { ObjectType::Directory } else { ObjectType::File }
				});
				
				reply.written(data.len() as u32);
				return;
			}
			reply.error(libc::ENOENT);
		});
	}

	fn create(&mut self, _req: &Request, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			let (uid, gid) = get_user_ids();
			let now = SystemTime::now();
			
			// Calculate current total size
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();

			// Account for metadata size (path and basic struct size)
			let metadata_size = std::mem::size_of::<crate::common::VirtualFile>() as u64 + name.len() as u64;
			if total_size + metadata_size > self.total_space_bytes {
				reply.error(libc::ENOSPC);
				return;
			}

			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			let file = crate::common::VirtualFile {
				content: Vec::new(),
				size: 0,
				is_directory: false,
				mtime: now,
			};

			let attr = FileAttr {
				ino: hash_path(&path),
				size: 0,
				blocks: 1,
				atime: now,
				mtime: now,
				ctime: now,
				crtime: now,
				kind: FileType::RegularFile,
				perm: 0o644,
				nlink: 1,
				uid,
				gid,
				rdev: 0,
				flags: 0,
				blksize: 512,
			};

			state.files.insert(path.clone(), file);
			state.emit_event(FSEvent::Created { path, object_type: ObjectType::File });
			
			reply.created(&TTL, &attr, 0, 0, 0);
		});
	}

	fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			
			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			if let Some(file) = state.files.remove(&path) {
				state.emit_event(FSEvent::Deleted { 
					path,
					object_type: file.get_type()
				});
				reply.ok();
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
		let (uid, gid) = get_user_ids();
		let now = SystemTime::now();
		
		if ino == 1 {
			let attr = FileAttr {
				ino: 1,
				size: 0,
				blocks: 0,
				atime: now,
				mtime: now,
				ctime: now,
				crtime: now,
				kind: FileType::Directory,
				perm: 0o755,
				nlink: 2,
				uid,
				gid,
				rdev: 0,
				flags: 0,
				blksize: 512,
			};
			reply.attr(&TTL, &attr);
			return;
		}

		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, file) in state.files.iter() {
				if hash_path(path) == ino {
					let attr = FileAttr {
						ino,
						size: file.size,
						blocks: 1,
						atime: file.mtime,
						mtime: file.mtime,
						ctime: file.mtime,
						crtime: file.mtime,
						kind: if file.is_directory { FileType::Directory } else { FileType::RegularFile },
						perm: if file.is_directory { 0o755 } else { 0o644 },
						nlink: if file.is_directory { 2 } else { 1 },
						uid,
						gid,
						rdev: 0,
						flags: 0,
						blksize: 512,
					};
					reply.attr(&TTL, &attr);
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn read(
		&mut self,
		_req: &Request,
		ino: u64,
		_fh: u64,
		offset: i64,
		size: u32,
		_flags: i32,
		_lock: Option<u64>,
		reply: ReplyData,
	) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, file) in state.files.iter() {
				if hash_path(path) == ino {
					let data = &file.content[offset as usize..std::cmp::min(file.content.len(), (offset + size as i64) as usize)];
					reply.data(data);
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn readdir(
		&mut self,
		_req: &Request,
		ino: u64,
		_fh: u64,
		offset: i64,
		mut reply: ReplyDirectory,
	) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;

			// Find the directory path for this inode
			let dir_path = if ino == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == ino) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOTDIR);
						return;
					}
				}
			};

			let mut entries = vec![
				(ino, FileType::Directory, "."),
				(if ino == 1 { 1 } else { hash_path(dir_path.rsplit('/').next().unwrap_or("")) }, FileType::Directory, ".."),
			];

			// Add entries in this directory
			for (path, file) in state.files.iter() {
				if path == &dir_path {
					continue;
				}

				let is_direct_child = if dir_path.is_empty() {
					!path.contains('/')
				} else {
					path.starts_with(&format!("{}/", dir_path)) && 
					path[dir_path.len()+1..].split('/').count() == 1
				};

				if is_direct_child {
					let name = path.split('/').last().unwrap();
					entries.push((
						hash_path(path),
						if file.is_directory { FileType::Directory } else { FileType::RegularFile },
						name,
					));
				}
			}

			for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
				if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
					break;
				}
			}
			reply.ok();
		});
	}

	fn setattr(
		&mut self,
		_req: &Request,
		ino: u64,
		mode: Option<u32>,
		uid: Option<u32>,
		gid: Option<u32>,
		size: Option<u64>,
		atime: Option<TimeOrNow>,
		mtime: Option<TimeOrNow>,
		_ctime: Option<SystemTime>,
		_fh: Option<u64>,
		_crtime: Option<SystemTime>,
		_chgtime: Option<SystemTime>,
		_bkuptime: Option<SystemTime>,
		_flags: Option<u32>,
		reply: ReplyAttr,
	) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			let (current_uid, current_gid) = get_user_ids();
			let now = SystemTime::now();

			let mut found_path = None;
			let mut found_attr = None;
			let mut should_emit_event = false;

			// Calculate current total size
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();

			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					found_path = Some(path.clone());
					break;
				}
			}

			if let Some(path) = found_path {
				let mut is_dir = false;
				if let Some(file) = state.files.get_mut(&path) {
					// Handle file size changes (truncation)
					if let Some(new_size) = size {
						// Check if this size change would exceed the limit
						let size_change = if new_size > file.size {
							new_size - file.size
						} else {
							0
						};

						if total_size + size_change > self.total_space_bytes {
							reply.error(libc::ENOSPC);
							return;
						}

						file.content.resize(new_size as usize, 0);
						file.size = new_size;
						file.mtime = now;
						should_emit_event = true;
					}

					// Handle mtime updates
					if let Some(mtime) = mtime {
						match mtime {
							TimeOrNow::Now => file.mtime = now,
							TimeOrNow::SpecificTime(time) => file.mtime = time,
						}
					}

					found_attr = Some(FileAttr {
						ino,
						size: file.size,
						blocks: 1,
						atime: match atime {
							Some(TimeOrNow::Now) => now,
							Some(TimeOrNow::SpecificTime(time)) => time,
							None => file.mtime,
						},
						mtime: file.mtime,
						ctime: file.mtime,
						crtime: file.mtime,
						kind: if file.is_directory { FileType::Directory } else { FileType::RegularFile },
						perm: mode.unwrap_or(if file.is_directory { 0o755 } else { 0o644 }) as u16,
						nlink: if file.is_directory { 2 } else { 1 },
						uid: uid.unwrap_or(current_uid),
						gid: gid.unwrap_or(current_gid),
						rdev: 0,
						flags: 0,
						blksize: 512,
					});

					if should_emit_event {
						is_dir = file.is_directory;
					}
				}

				if should_emit_event {
					state.emit_event(FSEvent::Modified { 
						path,
						object_type: if is_dir { ObjectType::Directory } else { ObjectType::File }
					});
				}
			}

			if let Some(attr) = found_attr {
				reply.attr(&TTL, &attr);
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					reply.opened(0, flags as u32);
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					reply.ok();
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					reply.ok();
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn release(&mut self, _req: &Request, ino: u64, _fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			for (path, _) in state.files.iter() {
				if hash_path(path) == ino {
					reply.ok();
					return;
				}
			}
			reply.error(libc::ENOENT);
		});
	}

	fn mkdir(&mut self, _req: &Request, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			let (uid, gid) = get_user_ids();
			let now = SystemTime::now();
			
			// Calculate current total size
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();

			// Account for directory metadata size (path and basic struct size)
			let metadata_size = std::mem::size_of::<crate::common::VirtualFile>() as u64 + name.len() as u64;
			if total_size + metadata_size > self.total_space_bytes {
				reply.error(libc::ENOSPC);
				return;
			}
			
			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			let dir = crate::common::VirtualFile {
				content: Vec::new(),
				size: metadata_size, // Store the metadata size for directories
				is_directory: true,
				mtime: now,
			};

			let attr = FileAttr {
				ino: hash_path(&path),
				size: 0,
				blocks: 1,
				atime: now,
				mtime: now,
				ctime: now,
				crtime: now,
				kind: FileType::Directory,
				perm: 0o755,
				nlink: 2,
				uid,
				gid,
				rdev: 0,
				flags: 0,
				blksize: 512,
			};

			state.files.insert(path.clone(), dir);
			state.emit_event(FSEvent::Created { path, object_type: ObjectType::Directory });
			
			reply.entry(&TTL, &attr, 0);
		});
	}

	fn rename(&mut self, _req: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			
			// Get parent paths
			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let new_parent_path = if newparent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == newparent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			// Construct old and new paths
			let old_path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			let new_path = if new_parent_path.is_empty() {
				newname.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", new_parent_path, newname.to_string_lossy())
			};

			// Get the file/directory being renamed
			if let Some(file) = state.files.remove(&old_path) {
				let is_dir = file.is_directory;
				
				// If it's a directory, we need to update all child paths
				if is_dir {
					let mut paths_to_rename = Vec::new();
					for (path, _) in state.files.iter() {
						if path.starts_with(&format!("{}/", old_path)) {
							paths_to_rename.push(path.clone());
						}
					}

					for old_child_path in paths_to_rename {
						if let Some(file) = state.files.remove(&old_child_path) {
							let new_child_path = old_child_path.replacen(&old_path, &new_path, 1);
							state.files.insert(new_child_path, file);
						}
					}
				}

				// Insert the renamed file/directory
				state.files.insert(new_path.clone(), file);
				
				// Emit events
				state.emit_event(FSEvent::Deleted {
					path: old_path,
					object_type: if is_dir { ObjectType::Directory } else { ObjectType::File }
				});
				state.emit_event(FSEvent::Created {
					path: new_path,
					object_type: if is_dir { ObjectType::Directory } else { ObjectType::File }
				});
				
				reply.ok();
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			
			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			// Check if directory exists and is actually a directory
			match state.files.get(&path) {
				Some(file) if !file.is_directory => {
					reply.error(libc::ENOTDIR);
					return;
				}
				None => {
					reply.error(libc::ENOENT);
					return;
				}
				_ => {}
			}

			// Check if directory is empty
			let has_children = state.files.iter().any(|(child_path, _)| {
				child_path != &path && child_path.starts_with(&format!("{}/", path))
			});

			if has_children {
				reply.error(libc::ENOTEMPTY);
				return;
			}

			// Remove the directory
			if state.files.remove(&path).is_some() {
				state.emit_event(FSEvent::Deleted {
					path,
					object_type: ObjectType::Directory
				});
				reply.ok();
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn symlink(&mut self, _req: &Request, parent: u64, name: &OsStr, link: &Path, reply: ReplyEntry) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let mut state = self.state.write().await;
			let (uid, gid) = get_user_ids();
			let now = SystemTime::now();
			
			// Calculate current total size
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();

			// Check if adding this symlink would exceed the limit
			let link_size = link.to_string_lossy().len() as u64;
			if total_size + link_size > self.total_space_bytes {
				reply.error(libc::ENOSPC);
				return;
			}
			
			let parent_path = if parent == 1 {
				String::new()
			} else {
				match state.files.iter().find(|(path, file)| file.is_directory && hash_path(path) == parent) {
					Some((path, _)) => path.clone(),
					None => {
						reply.error(libc::ENOENT);
						return;
					}
				}
			};

			let path = if parent_path.is_empty() {
				name.to_string_lossy().into_owned()
			} else {
				format!("{}/{}", parent_path, name.to_string_lossy())
			};

			// Create symlink content (store the target path)
			let symlink = crate::common::VirtualFile {
				content: link.to_string_lossy().as_bytes().to_vec(),
				size: link_size,
				is_directory: false,
				mtime: now,
			};

			let attr = FileAttr {
				ino: hash_path(&path),
				size: symlink.size,
				blocks: 1,
				atime: now,
				mtime: now,
				ctime: now,
				crtime: now,
				kind: FileType::Symlink,
				perm: 0o777,
				nlink: 1,
				uid,
				gid,
				rdev: 0,
				flags: 0,
				blksize: 512,
			};

			state.files.insert(path.clone(), symlink);
			state.emit_event(FSEvent::Created {
				path,
				object_type: ObjectType::File // Symlinks are treated as special files
			});
			
			reply.entry(&TTL, &attr, 0);
		});
	}

	fn readlink(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyData) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			
			for (path, file) in state.files.iter() {
				if hash_path(path) == ino {
					reply.data(&file.content);
					return;
				}
			}
			
			reply.error(libc::ENOENT);
		});
	}

	fn statfs(&mut self, _req: &Request, _ino: u64, reply: fuser::ReplyStatfs) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			
			// Calculate total size of all files
			let total_size: u64 = state.files.values()
				.map(|file| file.size)
				.sum();

			// Calculate number of files/directories
			let total_files = state.files.len() as u64;
			
			let block_size: u64 = 4096; // 4KB blocks
			let total_blocks = self.total_space_bytes / block_size;
			let used_blocks = (total_size + block_size - 1) / block_size; // Round up
			let free_blocks = total_blocks.saturating_sub(used_blocks);
			
			reply.statfs(
				total_blocks,
				free_blocks,
				free_blocks, // Available blocks (same as free for this virtual fs)
				self.max_files, // Total files/inodes
				self.max_files.saturating_sub(total_files), // Free inodes
				block_size as u32,
				255, // Maximum name length
				0,   // Fragment size (unused)
			);
		});
	}
}

fn hash_path(path: &str) -> u64 {
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	let mut hasher = DefaultHasher::new();
	path.hash(&mut hasher);
	hasher.finish()
} 