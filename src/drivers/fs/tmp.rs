//! tmpfs – a very small, in-memory filesystem implementation.
//!
//! This follows the same general structure as `devfs`, but instead of exposing
//! devices it simply stores regular files and directories backed by RAM.  The
//! goal is to provide a writable filesystem that does not require any block
//! device.
//
//! A few caveats / simplifying assumptions:
//!   * No support for symlinks, special files, or hard-link accounting.
//!   * Metadata such as timestamps are largely ignored for now.
//!   * Concurrency correctness relies on the `SpinLock` implementation provided
//!     by the kernel crate (fine for a single-core system).
//!
//! Even with these simplifications, tmpfs is extremely useful as `/tmp` or even
//! the initial root filesystem during bring-up.

use crate::drivers::{Driver, FilesystemDriver};
use crate::sync::SpinLock;

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use async_trait::async_trait;
use core::sync::atomic::{AtomicU64, Ordering};
use libkernel::{
    error::{FsError, KernelError, Result},
    fs::{
        attr::{FileAttr, FilePermissions},
        BlockDevice, DirStream, Dirent, FileType, Filesystem, Inode, InodeId,
    },
};
use log::warn;

/// A single mounted tmpfs instance.
pub struct TmpFs {
    root: Arc<TmpInode>,
    next_inode_id: AtomicU64,
    fs_id: u64,
}

impl TmpFs {
    fn new(fs_id: u64) -> Arc<Self> {
        let root_inode = Arc::new(TmpInode {
            id: InodeId::from_fsid_and_inodeid(fs_id, 0),
            attr: SpinLock::new(FileAttr {
                file_type: FileType::Directory,
                mode: FilePermissions::from_bits_retain(0o755),
                ..FileAttr::default()
            }),
            kind: InodeKind::Directory(SpinLock::new(BTreeMap::new())),
        });

        Arc::new(Self {
            root: root_inode,
            next_inode_id: AtomicU64::new(1),
            fs_id,
        })
    }

    /// Allocates a brand-new inode number unique within this filesystem.
    fn allocate_inode(&self) -> InodeId {
        let ino = self.next_inode_id.fetch_add(1, Ordering::SeqCst);
        InodeId::from_fsid_and_inodeid(self.fs_id, ino)
    }
}

#[async_trait]
impl Filesystem for TmpFs {
    async fn root_inode(&self) -> Result<Arc<dyn Inode>> {
        Ok(self.root.clone())
    }

    fn id(&self) -> u64 {
        self.fs_id
    }
}

/// Variants for an individual tmpfs inode.
enum InodeKind {
    Directory(SpinLock<BTreeMap<String, Arc<TmpInode>>>),
    File { data: SpinLock<Vec<u8>> },
}

/// Simple `readdir` iterator.
struct TmpDirStream {
    entries: Vec<(String, Arc<TmpInode>)>,
    idx: usize,
}

#[async_trait]
impl DirStream for TmpDirStream {
    async fn next_entry(&mut self) -> Result<Option<Dirent>> {
        if self.idx >= self.entries.len() {
            return Ok(None);
        }

        let (name, inode) = &self.entries[self.idx];
        self.idx += 1;

        Ok(Some(Dirent {
            id: inode.id,
            name: name.clone(),
            file_type: inode.attr.lock_save_irq().file_type,
            offset: self.idx as u64,
        }))
    }
}

/// The actual in-memory inode object.
struct TmpInode {
    id: InodeId,
    attr: SpinLock<FileAttr>,
    kind: InodeKind,
}

#[async_trait]
impl Inode for TmpInode {
    fn id(&self) -> InodeId {
        self.id
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match &self.kind {
            InodeKind::File { data } => {
                let data = data.lock_save_irq();
                let start = core::cmp::min(offset as usize, data.len());
                let end = core::cmp::min(start + buf.len(), data.len());
                let slice = &data[start..end];
                buf[..slice.len()].copy_from_slice(slice);
                Ok(slice.len())
            }
            InodeKind::Directory(_) => Err(FsError::IsADirectory.into()),
        }
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        match &self.kind {
            InodeKind::File { data } => {
                let mut data = data.lock_save_irq();
                let end = offset as usize + buf.len();
                if end > data.len() {
                    data.resize(end, 0);
                }
                data[offset as usize..end].copy_from_slice(buf);
                self.attr.lock_save_irq().size = data.len() as u64;
                Ok(buf.len())
            }
            InodeKind::Directory(_) => Err(FsError::IsADirectory.into()),
        }
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        match &self.kind {
            InodeKind::File { data } => {
                let mut data = data.lock_save_irq();
                data.resize(size as usize, 0);
                self.attr.lock_save_irq().size = size;
                Ok(())
            }
            InodeKind::Directory(_) => Err(FsError::IsADirectory.into()),
        }
    }

    async fn getattr(&self) -> Result<FileAttr> {
        Ok(self.attr.lock_save_irq().clone())
    }

    async fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let children = children.lock_save_irq();
                children
                    .get(name)
                    .map(|ino| ino.clone() as Arc<dyn Inode>)
                    .ok_or_else(|| FsError::NotFound.into())
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }

    async fn create(
        &self,
        name: &str,
        file_type: FileType,
        permissions: u16,
    ) -> Result<Arc<dyn Inode>> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let mut children = children.lock_save_irq();
                if children.contains_key(name) {
                    return Err(KernelError::InUse);
                }

                // Locate the parent filesystem instance to allocate an inode.
                let fs = TMPFS_INSTANCES
                    .lock_save_irq()
                    .get(&self.id.fs_id())
                    .cloned()
                    .ok_or(FsError::InvalidFs)?;

                let new_inode_id = fs.allocate_inode();

                let new_kind = match file_type {
                    FileType::Directory => InodeKind::Directory(SpinLock::new(BTreeMap::new())),
                    _ => InodeKind::File {
                        data: SpinLock::new(Vec::new()),
                    },
                };

                let new_inode = Arc::new(TmpInode {
                    id: new_inode_id,
                    attr: SpinLock::new(FileAttr {
                        id: new_inode_id,
                        file_type,
                        mode: FilePermissions::from_bits_truncate(permissions),
                        ..FileAttr::default()
                    }),
                    kind: new_kind,
                });

                children.insert(name.to_string(), new_inode.clone());
                Ok(new_inode)
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }

    async fn unlink(&self, name: &str) -> Result<()> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let mut children = children.lock_save_irq();
                children
                    .remove(name)
                    .map(|_| ())
                    .ok_or_else(|| FsError::NotFound.into())
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }

    async fn readdir(&self, start_offset: u64) -> Result<Box<dyn DirStream>> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let mut vec: Vec<(String, Arc<TmpInode>)> =
                    children.lock_save_irq().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                vec.sort_by(|a, b| a.0.cmp(&b.0));

                Ok(Box::new(TmpDirStream {
                    entries: vec,
                    idx: start_offset as usize,
                }))
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }
}

/// The driver object that the broader driver framework interacts with.
pub struct TmpFsDriver;

impl TmpFsDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Driver for TmpFsDriver {
    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn as_filesystem_driver(self: Arc<Self>) -> Option<Arc<dyn FilesystemDriver>> {
        Some(self)
    }
}

#[async_trait]
impl FilesystemDriver for TmpFsDriver {
    async fn construct(
        &self,
        fs_id: u64,
        blk_dev: Option<Box<dyn BlockDevice>>,
    ) -> Result<Arc<dyn Filesystem>> {
        if blk_dev.is_some() {
            warn!("tmpfs is RAM-backed—ignoring provided block device");
        }

        let instance = TmpFs::new(fs_id);
        TMPFS_INSTANCES
            .lock_save_irq()
            .insert(fs_id, instance.clone());

        Ok(instance)
    }
}

/// A global registry of every tmpfs instance, keyed by filesystem id.  This is
/// required so that child inodes can find their parent filesystem to allocate
/// fresh inode numbers.
static TMPFS_INSTANCES: SpinLock<BTreeMap<u64, Arc<TmpFs>>> =
    SpinLock::new(BTreeMap::new());
