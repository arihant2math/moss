use super::Tid;
use crate::sync::OnceLock;
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use core::any::Any;
use core::time::Duration;
use libkernel::{
    error::Result,
    fs::{
        FileType, Filesystem, Inode, InodeId, PIDFS_ID,
        attr::{FileAttr, FilePermissions},
    },
    memory::PAGE_SIZE,
    proc::ids::{Gid, Uid},
};

pub struct PidFs {
    root: Arc<PidFsRootInode>,
}

impl PidFs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            root: Arc::new(PidFsRootInode {
                id: InodeId::from_fsid_and_inodeid(PIDFS_ID, 0),
            }),
        })
    }

    pub fn inode_for_pid(&self, pid: Tid, uid: Uid, gid: Gid, time: Duration) -> Arc<dyn Inode> {
        Arc::new(PidFsInode {
            attr: FileAttr {
                id: InodeId::from_fsid_and_inodeid(PIDFS_ID, u64::from(pid.value()) + 1),
                size: 0,
                block_size: PAGE_SIZE as u32,
                blocks: 0,
                atime: time,
                btime: time,
                mtime: time,
                ctime: time,
                file_type: FileType::File,
                permissions: FilePermissions::from_bits_retain(0o700),
                nlinks: 1,
                uid,
                gid,
            },
        })
    }
}

#[async_trait]
impl Filesystem for PidFs {
    async fn root_inode(&self) -> Result<Arc<dyn Inode>> {
        Ok(self.root.clone())
    }

    fn id(&self) -> u64 {
        PIDFS_ID
    }

    fn magic(&self) -> u64 {
        0x5049_4446 // PID_FS_MAGIC
    }
}

struct PidFsRootInode {
    id: InodeId,
}

#[async_trait]
impl Inode for PidFsRootInode {
    fn id(&self) -> InodeId {
        self.id
    }

    async fn getattr(&self) -> Result<FileAttr> {
        Ok(FileAttr {
            id: self.id,
            file_type: FileType::Directory,
            permissions: FilePermissions::from_bits_retain(0o555),
            block_size: PAGE_SIZE as u32,
            ..FileAttr::default()
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct PidFsInode {
    attr: FileAttr,
}

#[async_trait]
impl Inode for PidFsInode {
    fn id(&self) -> InodeId {
        self.attr.id
    }

    async fn getattr(&self) -> Result<FileAttr> {
        Ok(self.attr.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static PIDFS_INSTANCE: OnceLock<Arc<PidFs>> = OnceLock::new();

pub fn pidfs() -> Arc<PidFs> {
    PIDFS_INSTANCE.get_or_init(PidFs::new).clone()
}
