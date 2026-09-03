//! Unpacks a cpio archive into a tmpfs tree for the initramfs.
//! TODO: wtf do i do for device trees (`/dev/[blah]`)...

use crate::{
    error::{FsError, KernelError, Result},
    fs::{
        FileType, Inode,
        attr::FilePermissions,
        cpio::{CpioArchive, CpioFile},
        path::Path,
    },
    proc::ids::{Gid, Uid},
};
use alloc::{collections::BTreeMap, sync::Arc};
use core::{str, time::Duration};
use log::{debug, warn};

/// Mode for dirs that entries reference but the archive never lists.
const IMPLICIT_DIR_MODE: FilePermissions = FilePermissions::from_bits_retain(0o755);

/// A hardlink group (identifier key)
/// Every entry of the group has the same dev and inode#
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HLKey {
    dev: (u32, u32),
    ino: u32,
}

/// Write full contents of data into the file
async fn write_all(inode: &Arc<dyn Inode>, data: &[u8]) -> Result<()> {
    let mut written = 0;

    while written < data.len() {
        match inode.write_at(written as u64, &data[written..]).await? {
            0 => return Err(FsError::OutOfBounds.into()),
            n => written += n,
        }
    }

    Ok(())
}

/// Apply the file's attributes (owner, timestamps, etc) to the actual inode
async fn apply_attrs(inode: &Arc<dyn Inode>, file: &CpioFile<'_>) -> Result<()> {
    let mut attr = inode.getattr().await?;
    let mtime = Duration::from_secs(file.mtime as u64);

    attr.uid = Uid::new(file.uid);
    attr.gid = Gid::new(file.gid);
    attr.permissions = file.permissions();
    attr.atime = mtime;
    attr.mtime = mtime;
    attr.ctime = mtime;

    inode.setattr(attr).await
}

struct Unpacker {
    root: Arc<dyn Inode>,
    hardlinks: BTreeMap<HLKey, Arc<dyn Inode>>,
}

impl Unpacker {
    /// Walk down to a directory from the root.
    /// Create any component that isnt already part of the tree.
    async fn walk(&self, path: &str) -> Result<Arc<dyn Inode>> {
        let mut dir = self.root.clone();

        for comp in path.split('/').filter(|comp| !comp.is_empty()) {
            dir = match dir.lookup(comp).await {
                Ok(inode) => inode,
                Err(KernelError::Fs(FsError::NotFound)) => {
                    debug!("initramfs: creating unlisted directory {comp} of {path}");

                    dir.create(comp, FileType::Directory, IMPLICIT_DIR_MODE, None)
                        .await?
                }
                Err(e) => return Err(e),
            };
        }

        Ok(dir)
    }

    /// Create a regular file/link to one
    async fn create_reg(
        &mut self,
        parent: &Arc<dyn Inode>,
        name: &str,
        file: &CpioFile<'_>,
    ) -> Result<Arc<dyn Inode>> {
        // if ino (inode #) is 0, it means the caller gave us nothing and we simply defer
        let key = (file.nlink > 1 && file.ino != 0).then_some(HLKey {
            dev: file.dev,
            ino: file.ino,
        });

        let inode = match key.and_then(|key| self.hardlinks.get(&key).cloned()) {
            Some(inode) => {
                parent.link(name, inode.clone()).await?;
                inode
            }
            None => {
                let inode = parent
                    .create(name, FileType::File, file.permissions(), None)
                    .await?;

                if let Some(key) = key {
                    self.hardlinks.insert(key, inode.clone());
                }

                inode
            }
        };

        // only one entry of a hardlink group carries the data, and the rest
        // come through here with an empty slice, which write_all no-ops on
        write_all(&inode, file.data).await?;

        Ok(inode)
    }

    async fn unpack(&mut self, file: &CpioFile<'_>) -> Result<()> {
        // cpio cli thingy writes stuff like `./bin/bash`
        // which is kinda gee because when we walk we split on slashes which could be bad...
        // so we fix/sanitize the beginning of the path
        let path = file
            .name
            .strip_prefix("./")
            .or_else(|| file.name.strip_prefix('/'))
            .unwrap_or(file.name)
            .trim_end_matches('/');

        // the cpio archive's entry for its own root
        // which only has its attributes/state
        if path.is_empty() || path == "." {
            return apply_attrs(&self.root, file).await;
        }

        if path.split('/').any(|comp| matches!(comp, "" | "." | "..")) {
            warn!("initramfs: skipping unusable name {}", file.name);

            return Ok(());
        }

        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = self.walk(dir).await?;

        let inode = match file.file_type() {
            Some(FileType::Directory) => match parent
                .create(name, FileType::Directory, file.permissions(), None)
                .await
            {
                // if we walk to an earlier entry in the tree it will have been created anyways
                Err(KernelError::Fs(FsError::AlreadyExists)) => parent.lookup(name).await?,
                res => res?,
            },

            Some(FileType::File) => self.create_reg(&parent, name, file).await?,

            Some(FileType::Symlink) => {
                let Ok(target) = str::from_utf8(file.data) else {
                    warn!("initramfs: skipping symlink {path}, target is not UTF-8");

                    return Ok(());
                };

                return parent.symlink(name, Path::new(target)).await;
            }

            _ => {
                warn!("initramfs: skipping {path}, unsupported file type");

                return Ok(());
            }
        };

        apply_attrs(&inode, file).await
    }
}

/// Unpack a cpio archive into the tree branching off of the root node
pub async fn unpack_cpio(buf: &[u8], root: Arc<dyn Inode>) -> Result<()> {
    let mut unpacker = Unpacker {
        root,
        hardlinks: BTreeMap::new(),
    };

    for file in CpioArchive::new(buf) {
        unpacker.unpack(&file?).await?;
    }

    Ok(())
}
