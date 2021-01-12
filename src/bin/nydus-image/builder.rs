// Copyright 2020 Ant Financial. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Bootstrap and blob file builder for RAFS format

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{Error, Result};
use std::mem::size_of;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use sha2::digest::Digest;
use sha2::Sha256;

use nydus_utils::einval;
use rafs::metadata::digest::{self, RafsDigest};
use rafs::metadata::layout::*;
use rafs::metadata::{Inode, RafsMode, RafsStore, RafsSuper};
use rafs::storage::compress;
use rafs::{RafsIoRead, RafsIoWrite};

use crate::stargz;

use crate::node::*;
use crate::tree::Tree;

pub struct Builder {
    /// Source type: Directory | StargzIndex
    source_type: SourceType,
    /// Source path, for different source type:
    /// Directory: should be a directory path
    /// StargzIndex: should be a stargz index json file path
    source_path: PathBuf,
    /// Blob id (user specified or sha256(blob)).
    blob_id: String,
    /// Blob file writer.
    f_blob: Box<dyn RafsIoWrite>,
    /// Bootstrap file writer.
    f_bootstrap: Box<dyn RafsIoWrite>,
    /// Parent bootstrap file reader.
    f_parent_bootstrap: Option<Box<dyn RafsIoRead>>,
    /// Blob chunk compress flag.
    compressor: compress::Algorithm,
    /// Inode and chunk digest algorithm flag.
    digester: digest::Algorithm,
    /// Save host uid gid in each inode.
    explicit_uidgid: bool,
    /// Cache node index for hardlinks, HashMap<Inode, Vec<index>>.
    lower_inode_map: HashMap<(Inode, u64), Vec<u64>>,
    upper_inode_map: HashMap<(Inode, u64), Vec<u64>>,
    /// Store all chunk digest for chunk deduplicate during build.
    chunk_cache: HashMap<RafsDigest, OndiskChunkInfo>,
    /// Store all blob id entry during build.
    blob_table: OndiskBlobTable,
    /// Readahead file list, use BTreeMap to keep stable iteration order, HashMap<path, Option<index>>.
    readahead_files: BTreeMap<PathBuf, Option<u64>>,
    /// Specify files or directories which need to prefetch. Their inode indexes will
    /// be persist to prefetch table.
    hint_readahead_files: BTreeMap<PathBuf, Option<u64>>,
    prefetch_policy: PrefetchPolicy,
    /// Store all nodes during build, node index of root starting from 1,
    /// so the collection index equal to (node.index - 1).
    nodes: Vec<Node>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrefetchPolicy {
    None,
    /// Readahead will be issued from Fs layer, which leverages inode/chunkinfo to prefetch data
    /// from blob no mather where it resides(OSS/Localfs). Basically, it is willing to cache the
    /// data into blobcache(if exists). It's more nimble. With this policy applied, image builder
    /// currently puts readahead files' data into a continuous region within blob which behaves very
    /// similar to `Blob` policy.
    Fs,
    /// Readahead will be issued directly from backend/blob layer
    Blob,
}

impl FromStr for PrefetchPolicy {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "none" => Ok(Self::None),
            "fs" => Ok(Self::Fs),
            "blob" => Ok(Self::Blob),
            _ => Err(einval!("Invalid ra-policy string got.")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceType {
    Directory,
    StargzIndex,
}

impl FromStr for SourceType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "directory" => Ok(Self::Directory),
            "stargz_index" => Ok(Self::StargzIndex),
            _ => Err(einval!("Invalid source type.")),
        }
    }
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_type: SourceType,
        source_path: &Path,
        blob_path: &Path,
        bootstrap_path: &Path,
        parent_bootstrap_path: &Path,
        blob_id: String,
        compressor: compress::Algorithm,
        digester: digest::Algorithm,
        hint_readahead_files: BTreeMap<PathBuf, Option<u64>>,
        prefetch_policy: PrefetchPolicy,
        explicit_uidgid: bool,
    ) -> Result<Builder> {
        let f_blob = Box::new(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(blob_path)?,
        );
        let f_bootstrap = Box::new(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(bootstrap_path)?,
        );

        let f_parent_bootstrap: Option<Box<dyn RafsIoRead>> =
            if parent_bootstrap_path != Path::new("") {
                Some(Box::new(
                    OpenOptions::new()
                        .read(true)
                        .write(false)
                        .open(parent_bootstrap_path)?,
                ))
            } else {
                None
            };

        Ok(Builder {
            source_type,
            source_path: PathBuf::from(source_path),
            blob_id,
            f_blob,
            f_bootstrap,
            f_parent_bootstrap,
            compressor,
            digester,
            explicit_uidgid,
            lower_inode_map: HashMap::new(),
            upper_inode_map: HashMap::new(),
            chunk_cache: HashMap::new(),
            blob_table: OndiskBlobTable::new(),
            readahead_files: BTreeMap::new(),
            hint_readahead_files,
            prefetch_policy,
            nodes: Vec::new(),
        })
    }

    /// Gain file or directory inode indexes which will be put into prefetch table.
    fn need_prefetch(&mut self, node: &Node) -> bool {
        let path = &node.rootfs();
        let index = node.inode.i_ino;

        if self.prefetch_policy == PrefetchPolicy::None || node.inode.i_size == 0 {
            return false;
        }

        for f in self.hint_readahead_files.keys() {
            // As path is canonicalized, it should be reliable.
            if path.as_os_str() == f.as_os_str() {
                if self.prefetch_policy == PrefetchPolicy::Fs {
                    if let Some(i) = self.hint_readahead_files.get_mut(path) {
                        *i = Some(index);
                    }
                }
                return true;
            } else if path.starts_with(f) {
                // Users can specify hinted parent directory with its child files hinted as well.
                // Only put the parent directory into ondisk prefetch table since a hinted directory's
                // all child files will be prefetched after mount.
                if self.hint_readahead_files.get(path).is_some() {
                    self.hint_readahead_files.remove(path);
                }
                return true;
            }
        }

        false
    }

    /// Traverse node tree, set inode index, ino, child_index and
    /// child_count etc according to RAFS format, then store to nodes collection.
    fn build_rafs(&mut self, tree: &mut Tree, nodes: &mut Vec<Node>) -> Result<()> {
        let index = nodes.len() as u64;
        let parent = &mut nodes[tree.node.index as usize - 1];

        if parent.is_dir() {
            parent.inode.i_child_index = index as u32 + 1;
            parent.inode.i_child_count = tree.children.len() as u32;
        }

        let parent_ino = parent.inode.i_ino;

        // Cache dir tree for BFS walk
        let mut dirs: Vec<&mut Tree> = Vec::new();

        // Sort children list by name,
        // so that we can improve performance in fs read_dir using binary search.
        tree.children
            .sort_by_key(|child| child.node.name().to_os_string());

        for child in tree.children.iter_mut() {
            let index = nodes.len() as u64 + 1;
            child.node.index = index;
            child.node.inode.i_parent = parent_ino;

            // Hardlink handle, all hardlink nodes' ino, nlink should be the same,
            // because the real_ino may be conflicted between different layers,
            // so we need to find hardlink node index list in the layer where the node is located.
            let inode_map = if child.node.overlay.lower_layer() {
                &mut self.lower_inode_map
            } else {
                &mut self.upper_inode_map
            };
            if let Some(indexes) = inode_map.get_mut(&(child.node.real_ino, child.node.dev)) {
                indexes.push(index);
                let first_index = indexes.first().unwrap();
                let nlink = indexes.len() as u32;
                child.node.inode.i_ino = *first_index;
                child.node.inode.i_nlink = nlink;
                // Update nlink for previous hardlink inodes
                for idx in indexes {
                    if index == *idx {
                        continue;
                    }
                    nodes[*idx as usize - 1].inode.i_nlink = nlink;
                }
            } else {
                child.node.inode.i_ino = index;
                child.node.inode.i_nlink = 1;
                // Store inode real ino
                inode_map.insert(
                    (child.node.real_ino, child.node.dev),
                    vec![child.node.index],
                );
            }

            // Store node for bootstrap & blob dump.
            // Put the whiteout file of upper layer in the front of node list for layered build,
            // so that it can be applied to the node tree of lower layer first than other files of upper layer.
            if self.f_parent_bootstrap.is_some() && child.node.whiteout_type().is_some() {
                nodes.insert(0, child.node.clone());
            } else {
                nodes.push(child.node.clone());
            }

            if self.need_prefetch(&child.node) {
                self.readahead_files
                    .insert(child.node.rootfs(), Some(child.node.index));
            }

            // Store chunk for chunk deduplicate
            if child.node.is_reg() {
                for chunk in &child.node.chunks {
                    self.chunk_cache.insert(chunk.block_id, *chunk);
                }
            }

            if child.node.is_dir() {
                dirs.push(child);
            }
        }

        // According to filesystem semantics, a parent dir should have nlink equal to
        // 2 plus the number of its child directory. And in case of layered build,
        // updating parent directory's nlink here is reliable since builder re-constructs
        // the entire tree and intends to layout all inodes into a plain array fetching
        // from the previously applied tree.
        if tree.node.is_dir() {
            let parent_dir = &mut nodes[tree.node.index as usize - 1];
            parent_dir.inode.i_nlink = (2 + dirs.len()) as u32;
        }

        for dir in dirs {
            self.build_rafs(dir, nodes)?;
        }

        Ok(())
    }

    fn build_rafs_wrap(&mut self, mut tree: &mut Tree) -> Result<()> {
        let index = RAFS_ROOT_INODE;
        tree.node.index = index;
        tree.node.inode.i_ino = index;

        // Filesystem walking skips root inode within subsequent while loop, however, we allow
        // user to pass the source root as prefetch hint. Check it here.
        let root_path = Path::new("/").to_path_buf();
        if self.need_prefetch(&tree.node) {
            self.readahead_files.insert(root_path, Some(index));
        }

        let mut nodes = vec![tree.node.clone()];
        self.build_rafs(&mut tree, &mut nodes)?;
        self.nodes = nodes;

        Ok(())
    }

    /// Apply new node (upper layer from filesystem directory) to
    /// bootstrap node tree (lower layer from bootstrap file)
    pub fn apply_to_bootstrap(&mut self) -> Result<()> {
        let mut rs = RafsSuper {
            mode: RafsMode::Direct,
            digest_validate: true,
            ..Default::default()
        };

        rs.load(self.f_parent_bootstrap.as_mut().unwrap())?;

        let lower_compressor = rs.meta.get_compressor();
        if self.compressor != lower_compressor {
            return Err(einval!(format!(
                "Inconsistent compressor with the lower layer, current {}, lower: {}.",
                self.compressor, lower_compressor
            )));
        }

        // Reuse lower layer blob table,
        // we need to append the blob entry of upper layer to the table
        self.blob_table = rs.inodes.get_blob_table().as_ref().clone();

        // Build node tree of lower layer from a bootstrap file
        let mut tree = Tree::from_bootstrap(&rs)?;

        // Apply new node (upper layer) to node tree (lower layer)
        for node in &self.nodes {
            tree.apply(&node, true)?;
        }

        self.lower_inode_map.clear();
        self.upper_inode_map.clear();
        self.readahead_files.clear();
        self.build_rafs_wrap(&mut tree)?;

        Ok(())
    }

    /// Build node tree of upper layer from a filesystem directory
    pub fn build_from_filesystem(&mut self, layered: bool) -> Result<()> {
        let mut tree = Tree::from_filesystem(&self.source_path, self.explicit_uidgid, layered)?;

        self.build_rafs_wrap(&mut tree)?;

        Ok(())
    }

    /// Build node tree of upper layer from a stargz index
    pub fn build_from_stargz_index(&mut self) -> Result<()> {
        let mut tree =
            Tree::from_stargz_index(&self.source_path, &self.blob_id, self.explicit_uidgid)?;
        self.build_rafs_wrap(&mut tree)?;
        Ok(())
    }

    /// Dump blob file and generate chunks
    fn dump_blob(&mut self) -> Result<(Sha256, usize, usize)> {
        // Sort readahead list by file size for better prefetch
        let mut readahead_files = self
            .readahead_files
            .values()
            .filter_map(|index| index.as_ref())
            .collect::<Vec<&u64>>();
        readahead_files.sort_by_key(|index| self.nodes[**index as usize - 1].inode.i_size);

        let blob_index = self.blob_table.entries.len() as u32;

        let mut blob_readahead_size = 0usize;
        let mut blob_size = 0usize;
        let mut compress_offset = 0u64;
        let mut decompress_offset = 0u64;
        let mut blob_hash = Sha256::new();

        match self.source_type {
            SourceType::Directory => {
                // Dump readahead nodes
                for index in &readahead_files {
                    let node = self.nodes.get_mut(**index as usize - 1).unwrap();
                    debug!("[{}]\treadahead {}", node.overlay, node);
                    if node.overlay == Overlay::UpperAddition
                        || node.overlay == Overlay::UpperModification
                    {
                        blob_readahead_size += node.dump_blob(
                            &mut self.f_blob,
                            &mut blob_hash,
                            &mut compress_offset,
                            &mut decompress_offset,
                            &mut self.chunk_cache,
                            self.compressor,
                            self.digester,
                            blob_index,
                        )?;
                    }
                }

                blob_size += blob_readahead_size;

                // Dump other nodes
                for node in &mut self.nodes {
                    if self.readahead_files.get(&node.rootfs()).is_some() {
                        continue;
                    }
                    // Ignore lower layer node when dump blob
                    debug!("[{}]\t{}", node.overlay, node);
                    if !node.is_dir()
                        && (node.overlay == Overlay::UpperAddition
                            || node.overlay == Overlay::UpperModification)
                    {
                        blob_size += node.dump_blob(
                            &mut self.f_blob,
                            &mut blob_hash,
                            &mut compress_offset,
                            &mut decompress_offset,
                            &mut self.chunk_cache,
                            self.compressor,
                            self.digester,
                            blob_index,
                        )?;
                    }
                }
            }
            SourceType::StargzIndex => {
                // Set blob index and inode digest for upper nodes
                for node in &mut self.nodes {
                    if node.overlay.lower_layer() {
                        continue;
                    }

                    let mut inode_hasher = RafsDigest::hasher(digest::Algorithm::Sha256);

                    for chunk in node.chunks.iter_mut() {
                        (*chunk).blob_index = blob_index;
                        inode_hasher.digest_update(chunk.block_id.as_ref());
                    }

                    if node.is_symlink() {
                        node.inode.i_digest = RafsDigest::from_buf(
                            node.symlink.as_ref().unwrap().as_bytes(),
                            digest::Algorithm::Sha256,
                        );
                    } else {
                        node.inode.i_digest = inode_hasher.digest_finalize();
                    }
                }
            }
        }

        Ok((blob_hash, blob_size, blob_readahead_size))
    }

    /// Dump bootstrap and blob file, return (Vec<blob_id>, blob_size)
    fn dump_to_file(&mut self) -> Result<(Vec<String>, usize)> {
        let (blob_hash, blob_size, mut blob_readahead_size) = self.dump_blob()?;

        // Set blob hash as blob id if not specified.
        if !self.blob_id.is_empty() {
            self.blob_id = format!("{:x}", blob_hash.finalize());
        }
        if blob_size > 0
            || (self.source_type == SourceType::StargzIndex && !self.blob_id.is_empty())
        {
            if self.prefetch_policy != PrefetchPolicy::Blob {
                blob_readahead_size = 0;
            }
            self.blob_table
                .add(self.blob_id.clone(), 0, blob_readahead_size as u32);
        }

        // Set inode digest, use reverse iteration order to reduce repeated digest calculations.
        for idx in (0..self.nodes.len()).rev() {
            self.nodes[idx].inode.i_digest = self.digest_node(&self.nodes[idx])?;
        }

        // Set inode table
        let super_block_size = size_of::<OndiskSuperBlock>();
        let inode_table_entries = self.nodes.len() as u32;
        let mut inode_table = OndiskInodeTable::new(inode_table_entries as usize);
        let inode_table_size = inode_table.size();

        // Set prefetch table
        let mut prefetch_table = PrefetchTable::new();
        let mut prefetch_table_size = 0;
        let prefetch_table_entries = if self.prefetch_policy == PrefetchPolicy::Fs {
            for i in self
                .hint_readahead_files
                .iter()
                .filter_map(|(_, v)| v.as_ref())
            {
                prefetch_table.add_entry(*i as u32);
            }
            prefetch_table_size = prefetch_table.size();
            prefetch_table.len() as u32
        } else {
            0u32
        };

        // Set blob table, use sha256 string (length 64) as blob id if not specified
        let blob_table_size = self.blob_table.size();
        let prefetch_table_offset = super_block_size + inode_table_size;
        let blob_table_offset = (prefetch_table_offset + prefetch_table_size) as u64;

        // Set super block
        let mut super_block = OndiskSuperBlock::new();
        let inodes_count = (self.lower_inode_map.len() + self.upper_inode_map.len()) as u64;
        super_block.set_inodes_count(inodes_count);
        super_block.set_inode_table_offset(super_block_size as u64);
        super_block.set_inode_table_entries(inode_table_entries);
        super_block.set_blob_table_offset(blob_table_offset);
        super_block.set_blob_table_size(blob_table_size as u32);
        super_block.set_prefetch_table_offset(prefetch_table_offset as u64);
        super_block.set_compressor(self.compressor);
        super_block.set_digester(self.digester);
        if self.explicit_uidgid {
            super_block.set_explicit_uidgid();
        }
        if self.source_type == SourceType::StargzIndex {
            super_block.set_block_size(stargz::DEFAULT_BLOCK_SIZE);
        }
        super_block.set_prefetch_table_entries(prefetch_table_entries);

        let mut inode_offset =
            (super_block_size + inode_table_size + prefetch_table_size + blob_table_size) as u32;

        let mut has_xattr = false;
        for node in &mut self.nodes {
            inode_table.set(node.index, inode_offset)?;
            // Add inode size
            inode_offset += node.inode.size() as u32;
            if node.inode.has_xattr() {
                has_xattr = true;
                if !node.xattrs.pairs.is_empty() {
                    inode_offset += (size_of::<OndiskXAttrs>() + node.xattrs.aligned_size()) as u32;
                }
            }
            // Add chunks size
            if node.is_reg() {
                inode_offset +=
                    (node.inode.i_child_count as usize * size_of::<OndiskChunkInfo>()) as u32;
            }
        }
        if has_xattr {
            super_block.set_has_xattr();
        }

        // Dump bootstrap
        super_block.store(&mut self.f_bootstrap)?;
        inode_table.store(&mut self.f_bootstrap)?;
        if self.prefetch_policy == PrefetchPolicy::Fs {
            prefetch_table.store(&mut self.f_bootstrap)?;
        }
        self.blob_table.store(&mut self.f_bootstrap)?;

        for node in &mut self.nodes {
            if self.source_type == SourceType::StargzIndex {
                debug!("[{}]\t{}", node.overlay, node);
                if log::max_level() >= log::LevelFilter::Debug {
                    for chunk in node.chunks.iter_mut() {
                        trace!("\t\tbuilding chunk: {}", chunk);
                    }
                }
            }
            node.dump_bootstrap(&mut self.f_bootstrap)?;
        }

        let blob_ids: Vec<String> = self
            .blob_table
            .entries
            .iter()
            .map(|entry| entry.blob_id.clone())
            .collect();

        Ok((blob_ids, blob_size))
    }

    /// Calculate inode digest
    fn digest_node(&self, node: &Node) -> Result<RafsDigest> {
        // We have set digest for non-directory inode in the previous dump_blob workflow, so just return digest here.
        if !node.is_dir() {
            return Ok(node.inode.i_digest);
        }

        let child_index = node.inode.i_child_index;
        let child_count = node.inode.i_child_count;
        let mut inode_hasher = RafsDigest::hasher(self.digester);

        for idx in child_index..child_index + child_count {
            let child = &self.nodes[(idx - 1) as usize];
            inode_hasher.digest_update(child.inode.i_digest.as_ref());
        }

        let inode_hash = inode_hasher.digest_finalize();

        Ok(inode_hash)
    }

    /// Build workflow, return (Vec<blob_id>, blob_size)
    pub fn build(&mut self) -> Result<(Vec<String>, usize)> {
        match self.source_type {
            SourceType::Directory => {
                self.build_from_filesystem(self.f_parent_bootstrap.is_some())?;
            }
            SourceType::StargzIndex => {
                self.build_from_stargz_index()?;
            }
        }
        if self.f_parent_bootstrap.is_some() {
            // For layered build
            self.apply_to_bootstrap()?;
        }
        // Dump blob and bootstrap file
        let (blob_ids, blob_size) = self.dump_to_file()?;

        Ok((blob_ids, blob_size))
    }
}
