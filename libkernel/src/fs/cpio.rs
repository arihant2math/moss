//! Parser for cpio archives for initramfs.
//!

use crate::{
    driver::CharDevDescriptor,
    error::{FsError, Result},
    fs::{FileType, attr::FilePermissions},
};
use core::{ptr, str};

const CPIO_HEADER_LEN: usize = 110;
const CPIO_TRAILER: &str = "TRAILER!!!";

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct CpioHeader {
    magic: [u8; 6],
    ino: [u8; 8],
    mode: [u8; 8],
    uid: [u8; 8],
    gid: [u8; 8],
    nlink: [u8; 8],
    mtime: [u8; 8],
    filesize: [u8; 8],
    devmajor: [u8; 8],
    devminor: [u8; 8],
    rdevmajor: [u8; 8],
    rdevminor: [u8; 8],
    namesize: [u8; 8],
    check: [u8; 8],
}

pub struct CpioArchive<'a> {
    data: &'a [u8],
    pos: usize,
    done: bool,
}

/// A file in the cpio archive
/// Also used to represent the terminator.
pub struct CpioFile<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub nlink: u32,
    pub ino: u32,
    pub dev: (u32, u32),
    pub rdev: (u32, u32),
}

// there is robably a better way to do these
fn parse_hex(field: &[u8; 8]) -> Result<u32> {
    let s = str::from_utf8(field).map_err(|_| FsError::CpioError)?;

    u32::from_str_radix(s, 16).map_err(|_| FsError::CpioError.into())
}
fn align4(offset: usize) -> Result<usize> {
    Ok(offset.checked_add(3).ok_or(FsError::CpioError)? & !3)
}

/// Returns `true` if the buffer starts with a cpio header.
pub fn is_cpio(buf: &[u8]) -> bool {
    buf.starts_with(b"070701")
}

impl<'a> CpioArchive<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            data: buf,
            pos: 0,
            done: false,
        }
    }

    /// Parse the next file in the archive
    /// whatever is claling this needs to abort on finding the trailer
    fn get_next_file(&mut self) -> Result<CpioFile<'a>> {
        let header_end = self
            .pos
            .checked_add(CPIO_HEADER_LEN)
            .ok_or(FsError::CpioError)?;
        let header_buf = self
            .data
            .get(self.pos..header_end)
            .ok_or(FsError::CpioError)?;

        let header: CpioHeader = unsafe { ptr::read_unaligned(header_buf.as_ptr() as *const _) };

        if !is_cpio(&header.magic) {
            return Err(FsError::CpioError.into());
        }

        let fnamesize = parse_hex(&header.namesize)? as usize;
        let fsize = parse_hex(&header.filesize)? as usize;

        let name_end = header_end
            .checked_add(fnamesize)
            .ok_or(FsError::CpioError)?;
        let name_buf = self
            .data
            .get(header_end..name_end)
            .ok_or(FsError::CpioError)?;

        let name = match name_buf.split_last() {
            Some((0, name)) => str::from_utf8(name).map_err(|_| FsError::CpioError)?,
            _ => return Err(FsError::CpioError.into()),
        };

        // there is 4 byte padding stuff apparently in cpio archives
        let data_start = align4(name_end)?;
        let data_end = data_start.checked_add(fsize).ok_or(FsError::CpioError)?;
        let data = self
            .data
            .get(data_start..data_end)
            .ok_or(FsError::CpioError)?;

        self.pos = align4(data_end)?;

        Ok(CpioFile {
            name,
            data,
            mode: parse_hex(&header.mode)?,
            uid: parse_hex(&header.uid)?,
            gid: parse_hex(&header.gid)?,
            mtime: parse_hex(&header.mtime)?,
            nlink: parse_hex(&header.nlink)?,
            ino: parse_hex(&header.ino)?,
            dev: (parse_hex(&header.devmajor)?, parse_hex(&header.devminor)?),
            rdev: (parse_hex(&header.rdevmajor)?, parse_hex(&header.rdevminor)?),
        })
    }
}

impl<'a> Iterator for CpioArchive<'a> {
    type Item = Result<CpioFile<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        match self.get_next_file() {
            Ok(file) if file.name == CPIO_TRAILER => {
                self.done = true;
                None
            }
            Ok(file) => Some(Ok(file)),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

impl CpioFile<'_> {
    /// Get inode type (e.g. file, dir, block device, etc...)
    pub fn file_type(&self) -> Option<FileType> {
        let rdev = CharDevDescriptor {
            major: self.rdev.0 as u64,
            minor: self.rdev.1 as u64,
        };

        match self.mode & 0o170000 {
            0o100000 => Some(FileType::File),
            0o040000 => Some(FileType::Directory),
            0o120000 => Some(FileType::Symlink),
            0o020000 => Some(FileType::CharDevice(rdev)),
            0o060000 => Some(FileType::BlockDevice(rdev)),
            0o010000 => Some(FileType::Fifo),
            _ => None,
        }
    }

    /// Get permission bits.
    pub fn permissions(&self) -> FilePermissions {
        FilePermissions::from_bits_retain((self.mode & 0o7777) as u16)
    }
}
