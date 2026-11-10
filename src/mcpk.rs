//! MCPK container reader.
//!
//! Layout (validated against both `vanilla.mcp` and `vanilla_old.mcp`):
//!
//! ```text
//! 0x00  "MCPK"
//! 0x0c  u32  dir_table_start   (absolute offset of the directory table)
//! 0x10  u32  file_table_start  (absolute offset of the file entry table)
//! 0x14  u32  data_start        (absolute offset of the first file body)
//!
//! directory table @ dir_table_start:  { dir_id: u32, offset: u32, count: u32 } *
//!   -> the directory's file entries live at file_table_start + offset
//! file entry table:                   { file_id: u32, offset: u32, size: u32, unknown: u32 } *
//!   -> file body = data_start + offset .. + size
//! ```
//!
//! `file_id` is `StringID(name)` — a lossy hash, so the real name has to come
//! from the payload itself (`co_filename`). Nothing about the layout is
//! version specific, which is why the reader only hard-codes the magic and
//! sanity-checks the three offsets.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"MCPK";
const HEADER_OFF_DIRS: usize = 0x0c;
const HEADER_OFF_FILES: usize = 0x10;
const HEADER_OFF_DATA: usize = 0x14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirId(pub u32);

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub dir: DirId,
    /// Position inside the directory's entry list.
    pub index: u32,
    pub id: FileId,
    pub offset: u32,
    pub size: u32,
    /// Fourth field of the entry: always 0 in the samples seen so far.
    pub unknown: u32,
}

impl FileEntry {
    pub fn range(&self, data_start: usize) -> std::ops::Range<usize> {
        let start = data_start + self.offset as usize;
        start..start + self.size as usize
    }
}

#[derive(Debug)]
pub struct Container {
    data: Vec<u8>,
    pub dir_table_start: u32,
    pub file_table_start: u32,
    pub data_start: u32,
    pub dirs: Vec<DirId>,
    pub files: Vec<FileEntry>,
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

impl Container {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_bytes(data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.len() < 0x18 {
            bail!("container is too small ({} bytes)", data.len());
        }
        if &data[..4] != MAGIC {
            bail!("bad magic: {:02x?} (expected MCPK)", &data[..4]);
        }
        let dir_table_start = u32_at(&data, HEADER_OFF_DIRS);
        let file_table_start = u32_at(&data, HEADER_OFF_FILES);
        let data_start = u32_at(&data, HEADER_OFF_DATA);
        let len = data.len() as u32;
        for (name, val) in [
            ("dir_table_start", dir_table_start),
            ("file_table_start", file_table_start),
            ("data_start", data_start),
        ] {
            if val >= len {
                bail!("{name} = 0x{val:x} points outside the file (size 0x{len:x})");
            }
        }
        if !(dir_table_start <= file_table_start && file_table_start <= data_start) {
            bail!(
                "section offsets are not monotonic: dirs=0x{dir_table_start:x} files=0x{file_table_start:x} data=0x{data_start:x}"
            );
        }

        // Directory table runs from dir_table_start up to the file table.
        let dir_region = &data[dir_table_start as usize..file_table_start as usize];
        if dir_region.len() % 12 != 0 {
            bail!("directory table size {} is not a multiple of 12", dir_region.len());
        }
        let mut dirs = Vec::with_capacity(dir_region.len() / 12);
        let mut files = Vec::new();
        // Some builds have been seen to list the same file id from several
        // directories; keep the first occurrence so each body is written once.
        let mut seen: BTreeMap<(u32, u32), ()> = BTreeMap::new();
        for chunk in dir_region.chunks_exact(12) {
            let dir_id = u32_at(chunk, 0);
            let offset = u32_at(chunk, 4) as usize;
            let count = u32_at(chunk, 8) as usize;
            dirs.push(DirId(dir_id));
            let start = file_table_start as usize + offset;
            let end = start + count.saturating_mul(16);
            if end > data.len() {
                bail!(
                    "directory 0x{dir_id:08x} entries 0x{start:x}..0x{end:x} exceed file size 0x{:x}",
                    data.len()
                );
            }
            for (index, entry) in data[start..end].chunks_exact(16).enumerate() {
                let id = u32_at(entry, 0);
                let e = FileEntry {
                    dir: DirId(dir_id),
                    index: index as u32,
                    id: FileId(id),
                    offset: u32_at(entry, 4),
                    size: u32_at(entry, 8),
                    unknown: u32_at(entry, 12),
                };
                if data_start as usize + e.offset as usize + e.size as usize > data.len() {
                    bail!(
                        "file 0x{:08x} in dir 0x{dir_id:08x} overruns the file (offset 0x{:x} size 0x{:x})",
                        e.id.0,
                        e.offset,
                        e.size
                    );
                }
                if seen.insert((dir_id, id), ()).is_none() {
                    files.push(e);
                }
            }
        }
        Ok(Container {
            data,
            dir_table_start,
            file_table_start,
            data_start,
            dirs,
            files,
        })
    }

    pub fn body(&self, entry: &FileEntry) -> &[u8] {
        &self.data[entry.range(self.data_start as usize)]
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_magic() {
        let err = Container::from_bytes(vec![0u8; 64]).unwrap_err().to_string();
        assert!(err.contains("bad magic"), "{err}");
    }
}
