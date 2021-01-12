// Copyright 2020 Alibaba cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Stargz support.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Result;
use std::path::PathBuf;
use std::rc::Rc;

use nydus_utils::einval;
use rafs::metadata::digest::{Algorithm, RafsDigest};

pub const DEFAULT_BLOCK_SIZE: u32 = 4 << 20;

type RcTocEntry = Rc<RefCell<TocEntry>>;

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct TocEntry {
    // Name is the tar entry's name. It is the complete path
    // stored in the tar file, not just the base name.
    pub name: PathBuf,

    // Type is one of "dir", "reg", "symlink", "hardlink", "char",
    // "block", "fifo", or "chunk".
    // The "chunk" type is used for regular file data chunks past the first
    // TOCEntry; the 2nd chunk and on have only Type ("chunk"), Offset,
    // ChunkOffset, and ChunkSize populated.
    #[serde(rename = "type")]
    pub toc_type: String,

    // Size, for regular files, is the logical size of the file.
    #[serde(default)]
    pub size: u64,

    // // ModTime3339 is the modification time of the tar entry. Empty
    // // means zero or unknown. Otherwise it's in UTC RFC3339
    // // format. Use the ModTime method to access the time.Time value.
    // #[serde(default, alias = "modtime")]
    // mod_time_3339: String,
    // #[serde(skip)]
    // mod_time: Time,

    // LinkName, for symlinks and hardlinks, is the link target.
    #[serde(default, rename = "linkName")]
    pub link_name: PathBuf,

    // Mode is the permission and mode bits.
    #[serde(default)]
    pub mode: u32,

    // Uid is the user ID of the owner.
    #[serde(default)]
    pub uid: u32,

    // Gid is the group ID of the owner.
    #[serde(default)]
    pub gid: u32,

    // Uname is the username of the owner.
    //
    // In the serialized JSON, this field may only be present for
    // the first entry with the same Uid.
    #[serde(default, rename = "userName")]
    pub uname: String,

    // Gname is the group name of the owner.
    //
    // In the serialized JSON, this field may only be present for
    // the first entry with the same Gid.
    #[serde(default, rename = "groupName")]
    pub gname: String,

    // Offset, for regular files, provides the offset in the
    // stargz file to the file's data bytes. See ChunkOffset and
    // ChunkSize.
    #[serde(default)]
    pub offset: u64,

    // the Offset of the next entry with a non-zero Offset
    #[serde(skip)]
    pub next_offset: u64,

    // DevMajor is the major device number for "char" and "block" types.
    #[serde(default, rename = "devMajor")]
    pub dev_major: u64,

    // DevMinor is the major device number for "char" and "block" types.
    #[serde(default, rename = "devMinor")]
    pub dev_minor: u64,

    // NumLink is the number of entry names pointing to this entry.
    // Zero means one name references this entry.
    #[serde(skip)]
    pub num_link: u32,

    // Xattrs are the extended attribute for the entry.
    #[serde(default)]
    pub xattrs: HashMap<String, String>,

    // Digest stores the OCI checksum for regular files payload.
    // It has the form "sha256:abcdef01234....".
    #[serde(default)]
    pub digest: String,

    // ChunkOffset is non-zero if this is a chunk of a large,
    // regular file. If so, the Offset is where the gzip header of
    // ChunkSize bytes at ChunkOffset in Name begin.
    //
    // In serialized form, a "chunkSize" JSON field of zero means
    // that the chunk goes to the end of the file. After reading
    // from the stargz TOC, though, the ChunkSize is initialized
    // to a non-zero file for when Type is either "reg" or
    // "chunk".
    #[serde(default, rename = "chunkOffset")]
    pub chunk_offset: u64,
    #[serde(default, rename = "chunkSize")]
    pub chunk_size: u64,

    #[serde(skip)]
    pub children: Vec<RcTocEntry>,

    #[serde(skip)]
    pub inode: u64,
}

impl TocEntry {
    pub fn is_dir(&self) -> bool {
        self.toc_type.as_str() == "dir"
    }

    pub fn is_reg(&self) -> bool {
        self.toc_type.as_str() == "reg"
    }

    pub fn is_symlink(&self) -> bool {
        self.toc_type.as_str() == "symlink"
    }

    pub fn is_hardlink(&self) -> bool {
        self.toc_type.as_str() == "hardlink"
    }

    pub fn is_chunk(&self) -> bool {
        self.toc_type.as_str() == "chunk"
    }

    pub fn has_xattr(&self) -> bool {
        !self.xattrs.is_empty()
    }

    pub fn mode(&self) -> u32 {
        let mut mode = self.mode;

        if self.is_dir() {
            mode |= libc::S_IFDIR;
        } else if self.is_reg() || self.is_hardlink() {
            mode |= libc::S_IFREG;
        } else if self.is_symlink() {
            mode |= libc::S_IFLNK;
        }

        mode
    }

    // Convert entry name to file name
    // For example: `` to `/`, `/` to `/`, `a/b` to `b`, `a/b/` to `b`
    pub fn name(&self) -> Result<PathBuf> {
        let path = self.path()?;
        let root_path = PathBuf::from("/");
        if path == root_path {
            return Ok(root_path);
        }
        let name = path
            .file_name()
            .ok_or_else(|| einval!("invalid entry name"))?;
        Ok(PathBuf::from(name))
    }

    // Convert entry name to rootfs absolute path
    // For example: `` to `/`, `a/b` to `/a/b`, `a/b/` to `/a/b`
    pub fn path(&self) -> Result<PathBuf> {
        let root_path = PathBuf::from("/");
        let empty_path = PathBuf::from("");
        if self.name == empty_path || self.name == root_path {
            return Ok(root_path);
        }
        let path = PathBuf::from("/").join(&self.name);
        Ok(path
            .parent()
            .ok_or_else(|| einval!("invalid entry path"))?
            .join(
                path.file_name()
                    .ok_or_else(|| einval!("invalid entry name"))?,
            ))
    }

    // Convert link path of hardlink entry to rootfs absolute path
    // For example: `a/b` to `/a/b`
    pub fn hardlink_link_path(&self) -> PathBuf {
        PathBuf::from("/").join(&self.link_name)
    }

    pub fn symlink_link_path(&self) -> PathBuf {
        self.link_name.clone()
    }

    pub fn is_supported(&self) -> bool {
        self.is_dir() || self.is_reg() || self.is_symlink() || self.is_hardlink() || self.is_chunk()
    }

    // TODO: think about chunk deduplicate
    pub fn block_id(&self, blob_id: &str) -> Result<RafsDigest> {
        if !self.is_reg() && !self.is_chunk() {
            return Err(einval!("only support chunk or reg entry"));
        }
        let data = serde_json::to_string(self)
            .map_err(|e| einval!(format!("block id calculation failed: {:?}", e)))?;
        Ok(RafsDigest::from_buf(
            (data + blob_id).as_bytes(),
            Algorithm::Sha256,
        ))
    }

    pub fn new_dir(path: PathBuf) -> Self {
        TocEntry {
            name: path,
            toc_type: String::from("dir"),
            mode: 0o755,
            num_link: 2,
            ..Default::default()
        }
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct TocIndex {
    pub version: u32,
    pub entries: Vec<TocEntry>,
}

pub fn parse_index(path: &PathBuf) -> Result<TocIndex> {
    let index_file = File::open(path)?;
    let toc_index: TocIndex = serde_json::from_reader(index_file)
        .map_err(|e| einval!(format!("invalid stargz index json file {:?}", e)))?;
    if toc_index.version != 1 {
        return Err(einval!(format!(
            "unsupported index version {}",
            toc_index.version
        )));
    }
    Ok(toc_index)
}
