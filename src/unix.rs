use crate::common::{SharedFSState, FSEvent, ObjectType};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use fuser::{
	FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
	Request, ReplyWrite, ReplyCreate,
};
use napi::bindgen_prelude::*;

const TTL: Duration = Duration::from_secs(1);

pub struct FSImpl {
	session: Option<fuser::BackgroundSession>,
	state: SharedFSState,
}

impl FSImpl {
	pub fn new(state: SharedFSState) -> Self {
		Self { 
			session: None,
			state,
		}
	}

	pub async fn mount(&mut self, mount_path: &Path) -> Result<()> {
		let options = vec![
			MountOption::FSName("virtual".to_string()),
			MountOption::AllowOther,
			MountOption::DefaultPermissions,
			MountOption::AutoUnmount,
		];
		
		let fs = VirtualFS {
			state: self.state.clone(),
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
}

impl Filesystem for VirtualFS {
	fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			
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
					uid: 1000,
					gid: 1000,
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
			
			// Find the file path and type by inode
			let (path, is_directory) = match state.files.iter().find(|(path, _)| hash_path(path) == ino) {
				Some((path, file)) => (path.clone(), file.is_directory),
				None => {
					reply.error(libc::ENOENT);
					return;
				}
			};

			// Get mutable reference and update content
			if let Some(file) = state.files.get_mut(&path) {
				let start = offset as usize;
				let end = start + data.len();
				
				if end > file.content.len() {
					file.content.resize(end, 0);
				}
				
				file.content[start..end].copy_from_slice(data);
				file.size = file.content.len() as u64;
				
				state.emit_event(FSEvent::Modified { 
					path,
					object_type: if is_directory { ObjectType::Directory } else { ObjectType::File }
				});
				
				reply.written(data.len() as u32);
			} else {
				reply.error(libc::ENOENT);
			}
		});
	}

	fn create(&mut self, _req: &Request, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
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

			let file = crate::common::VirtualFile {
				content: Vec::new(),
				size: 0,
				is_directory: false,
			};

			let attr = FileAttr {
				ino: hash_path(&path),
				size: 0,
				blocks: 1,
				atime: UNIX_EPOCH,
				mtime: UNIX_EPOCH,
				ctime: UNIX_EPOCH,
				crtime: UNIX_EPOCH,
				kind: FileType::RegularFile,
				perm: 0o644,
				nlink: 1,
				uid: 1000,
				gid: 1000,
				rdev: 0,
				flags: 0,
				blksize: 512,
			};

			state.files.insert(path.clone(), file);
			state.emit_event(FSEvent::Created { 
				path,
				object_type: ObjectType::File
			});
			
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
		if ino == 1 {
			let attr = FileAttr {
				ino: 1,
				size: 0,
				blocks: 0,
				atime: UNIX_EPOCH,
				mtime: UNIX_EPOCH,
				ctime: UNIX_EPOCH,
				crtime: UNIX_EPOCH,
				kind: FileType::Directory,
				perm: 0o755,
				nlink: 2,
				uid: 1000,
				gid: 1000,
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
						atime: UNIX_EPOCH,
						mtime: UNIX_EPOCH,
						ctime: UNIX_EPOCH,
						crtime: UNIX_EPOCH,
						kind: if file.is_directory { FileType::Directory } else { FileType::RegularFile },
						perm: if file.is_directory { 0o755 } else { 0o644 },
						nlink: if file.is_directory { 2 } else { 1 },
						uid: 1000,
						gid: 1000,
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
}

fn hash_path(path: &str) -> u64 {
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	let mut hasher = DefaultHasher::new();
	path.hash(&mut hasher);
	hasher.finish()
} 