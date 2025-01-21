use crate::common::{SharedFSState};
use std::ffi::OsStr;
use std::path::{Path};
use std::time::{Duration, UNIX_EPOCH};
use fuser::{
	FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
	Request,
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
			MountOption::RO,
			MountOption::FSName("virtual".to_string()),
			MountOption::AllowOther,
			MountOption::DefaultPermissions,
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
		let parent_path = if parent == 1 { "" } else { todo!() };
		let path = Path::new(parent_path).join(name);
		let path_str = path.to_string_lossy().into_owned();

		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			if let Some(file) = state.files.get(&path_str) {
				let attr = FileAttr {
					ino: hash_path(&path_str),
					size: file.size,
					blocks: 1,
					atime: UNIX_EPOCH,
					mtime: UNIX_EPOCH,
					ctime: UNIX_EPOCH,
					crtime: UNIX_EPOCH,
					kind: if file.is_directory { FileType::Directory } else { FileType::RegularFile },
					perm: 0o444,
					nlink: 1,
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
				perm: 0o555,
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
						perm: 0o444,
						nlink: 1,
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
		if ino != 1 {
			reply.error(libc::ENOTDIR);
			return;
		}

		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let state = self.state.read().await;
			let mut entries = vec![
				(1, FileType::Directory, "."),
				(1, FileType::Directory, ".."),
			];

			for (path, file) in state.files.iter() {
				if !path.contains('/') {  // Only root level entries
					entries.push((
						hash_path(path),
						if file.is_directory { FileType::Directory } else { FileType::RegularFile },
						path,
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