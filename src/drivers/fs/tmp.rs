use crate::drivers::{Driver, FilesystemDriver};
use crate::sync::{OnceLock, SpinLock};
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use async_trait::async_trait;
use core::sync::atomic::{AtomicU64, Ordering};
use libkernel::error::{FsError, KernelError, Result};
use libkernel::fs::attr::{FileAttr, FilePermissions};
use libkernel::fs::{
    BlockDevice, DEVFS_ID, DirStream, Dirent, FileType, Filesystem, Inode, InodeId,
};
use log::warn;

/// A unique, reserved ID for the tmpfs pseudo-filesystem.
///
/// We pick an ID that is currently unused by other pseudo filesystems.  If
/// additional pseudo filesystems are added in the future, consider grouping
/// them into a range similar to `DEVFS_ID`.
pub const TMPFS_ID: u64 = DEVFS_ID + 1;

/// The in-kernel instance of the tmpfs implementation.  Like devfs, there is
/// only ever one tmpfs: it is mounted at `/tmp` and backed purely by RAM.
pub struct TmpFs {
    root: Arc<TmpFsINode>,
    next_inode_id: AtomicU64,
}

impl TmpFs {
    /// Creates an empty tmpfs with a single root directory.
    fn new() -> Arc<Self> {
        let root_inode = Arc::new(TmpFsINode {
            id: InodeId::from_fsid_and_inodeid(TMPFS_ID, 0),
            attr: SpinLock::new(FileAttr {
                file_type: FileType::Directory,
                mode: FilePermissions::from_bits_retain(0o777), // World-writable tmp dir
                ..FileAttr::default()
            }),
            kind: InodeKind::Directory(SpinLock::new(BTreeMap::new())),
        });

        Arc::new(Self {
            root: root_inode,
            next_inode_id: AtomicU64::new(1),
        })
    }

    /// Allocates a new inode number within this tmpfs instance.
    fn alloc_inode_id(&self) -> InodeId {
        let id = self.next_inode_id.fetch_add(1, Ordering::SeqCst);
        InodeId::from_fsid_and_inodeid(TMPFS_ID, id)
    }
}

#[async_trait]
impl Filesystem for TmpFs {
    async fn root_inode(&self) -> Result<Arc<dyn Inode>> {
        Ok(self.root.clone())
    }

    fn id(&self) -> u64 {
        TMPFS_ID
    }
}

/// The different inode kinds tmpfs supports.
enum InodeKind {
    /// A POSIX directory with child entries.
    Directory(SpinLock<BTreeMap<String, Arc<TmpFsINode>>>),
    /// A regular file stored entirely in memory.
    File {
        /// File contents.  The length of the `Vec` is the current file size.
        data: SpinLock<Vec<u8>>,
    },
}

/// Simple directory stream that iterates over a snapshot of the children map.
struct TmpDirStreamer {
    children: Vec<(String, Arc<TmpFsINode>)>,
    idx: usize,
}

#[async_trait]
impl DirStream for TmpDirStreamer {
    async fn next_entry(&mut self) -> Result<Option<Dirent>> {
        if let Some((name, inode)) = self.children.get(self.idx) {
            self.idx += 1;
            Ok(Some(Dirent {
                id: inode.id,
                name: name.clone(),
                file_type: inode.attr.lock_save_irq().file_type,
                offset: self.idx as u64,
            }))
        } else {
            Ok(None)
        }
    }
}

/// An inode within tmpfs.
struct TmpFsINode {
    id: InodeId,
    attr: SpinLock<FileAttr>,
    kind: InodeKind,
}

#[async_trait]
impl Inode for TmpFsINode {
    fn id(&self) -> InodeId {
        self.id
    }

    /* ---------- File operations ---------- */

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match &self.kind {
            InodeKind::File { data } => {
                let data = data.lock_save_irq();
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }

                let end = core::cmp::min(start + buf.len(), data.len());
                buf[..end - start].copy_from_slice(&data[start..end]);
                Ok(end - start)
            }
            InodeKind::Directory(_) => Err(FsError::IsADirectory.into()),
        }
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        match &self.kind {
            InodeKind::File { data } => {
                let mut data = data.lock_save_irq();
                let start = offset as usize;
                let end = start + buf.len();

                if end > data.len() {
                    data.resize(end, 0);
                }

                data[start..end].copy_from_slice(buf);

                // Update file size in metadata
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

    /* ---------- Metadata operations ---------- */

    async fn getattr(&self) -> Result<FileAttr> {
        // Clone the cached attributes.  For files, make sure the size is up-to-date.
        let mut attr = self.attr.lock_save_irq().clone();
        if let InodeKind::File { data } = &self.kind {
            attr.size = data.lock_save_irq().len() as u64;
        }
        Ok(attr)
    }

    /* ---------- Directory operations ---------- */

    async fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let children = children.lock_save_irq();
                children
                    .get(name)
                    .map(|c| c.clone() as Arc<dyn Inode>)
                    .ok_or_else(|| FsError::NotFound.into())
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }

    async fn create(
        &self,
        name: &str,
        file_type: FileType,
        _permissions: u16,
    ) -> Result<Arc<dyn Inode>> {
        match &self.kind {
            InodeKind::Directory(children) => {
                let mut children = children.lock_save_irq();
                if children.contains_key(name) {
                    return Err(KernelError::InUse);
                }

                // Allocate an inode number **before** constructing the `Arc` so we can avoid
                // unsafe interior mutation later on.
                let inode_id = TMPFS_INSTANCE
                    .get()
                    .expect("tmpfs instance must be initialized")
                    .alloc_inode_id();

                let new_inode = Arc::new(TmpFsINode {
                    id: inode_id,
                    attr: SpinLock::new(FileAttr {
                        file_type,
                        mode: FilePermissions::from_bits_retain(0o666),
                        ..FileAttr::default()
                    }),
                    kind: match file_type {
                        FileType::Directory => InodeKind::Directory(SpinLock::new(BTreeMap::new())),
                        FileType::File => InodeKind::File {
                            data: SpinLock::new(Vec::new()),
                        },
                        _ => return Err(KernelError::NotSupported),
                    },
                });
                // Metadata in `attr` must reflect the freshly allocated inode id.
                new_inode.attr.lock_save_irq().id = inode_id;
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
                let snapshot: Vec<_> = children
                    .lock_save_irq()
                    .iter()
                    .map(|(n, i)| (n.clone(), i.clone()))
                    .collect();
                Ok(Box::new(TmpDirStreamer {
                    children: snapshot,
                    idx: start_offset as usize,
                }))
            }
            InodeKind::File { .. } => Err(FsError::NotADirectory.into()),
        }
    }
}

/// Driver object responsible for constructing tmpfs instances.
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
        _fs_id: u64,
        device: Option<Box<dyn BlockDevice>>,
    ) -> Result<Arc<dyn Filesystem>> {
        // tmpfs is purely in RAM and should not have any backing block device.
        if device.is_some() {
            warn!("tmpfs should have no backing store");
            return Err(KernelError::InvalidValue);
        }
        Ok(tmpfs())
    }
}

/* ---------- Global singleton helpers ---------- */

/// The single, global instance of tmpfs.
///
/// This mirrors the approach used in `devfs` and allows the VFS to mount
/// `/tmp` multiple times (if it really wanted to) while still referring to the
/// same underlying in-memory structures.
static TMPFS_INSTANCE: OnceLock<Arc<TmpFs>> = OnceLock::new();

/// Initializes (if necessary) and returns the global tmpfs instance.
pub fn tmpfs() -> Arc<TmpFs> {
    TMPFS_INSTANCE
        .get_or_init(|| {
            log::info!("tmpfs initialized");
            TmpFs::new()
        })
        .clone()
}
