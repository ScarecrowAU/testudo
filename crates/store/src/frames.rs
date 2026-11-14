//! Per-page frame recordings: the raw screencast JPEGs exactly as received, each
//! stamped with the hub receive time. No re-encoding while recording; video is
//! produced on demand by `export`.
//!
//! Browsers only emit a frame when the page repaints, so consecutive duplicates
//! are rare, but pages often return to an earlier exact state (spinners, cursor
//! blink, hover toggles, looping animations). Each distinct JPEG is stored once;
//! repeats are written as a 8-byte reference to the earlier payload.
//!
//! File format (little-endian):
//!
//! ```text
//! file   := MAGIC record*
//! record := f64 at_ms | u32 len | jpeg[len]             (len < REF_FLAG)
//!         | f64 at_ms | u32 (REF_FLAG | 8) | u64 offset  (repeat of the payload at offset)
//! ```
//!
//! A torn final record (crash mid-write) is ignored by the reader.

use std::collections::HashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hasher};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

const MAGIC: &[u8; 8] = b"TSTDFRM2";
const RECORD_HEADER: u64 = 12;
const REF_FLAG: u32 = 1 << 31;

pub struct FrameWriter {
    out: BufWriter<File>,
    pos: u64,
    /// (hash, len) of each distinct payload -> its offset in the file.
    seen: HashMap<(u64, u32), u64>,
    pub frames: u64,
    pub unique_frames: u64,
}

impl FrameWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = BufWriter::with_capacity(512 * 1024, File::create(path)?);
        out.write_all(MAGIC)?;
        Ok(Self {
            out,
            pos: MAGIC.len() as u64,
            seen: HashMap::new(),
            frames: 0,
            unique_frames: 0,
        })
    }

    pub fn append(&mut self, at: f64, jpeg: &[u8]) -> Result<()> {
        let len = u32::try_from(jpeg.len()).ok().filter(|l| *l < REF_FLAG).context("frame too large")?;
        let mut h = DefaultHasher::new();
        h.write(jpeg);
        let key = (h.finish(), len);

        self.out.write_all(&at.to_le_bytes())?;
        match self.seen.get(&key) {
            Some(&offset) => {
                self.out.write_all(&(REF_FLAG | 8).to_le_bytes())?;
                self.out.write_all(&offset.to_le_bytes())?;
                self.pos += RECORD_HEADER + 8;
            }
            None => {
                self.out.write_all(&len.to_le_bytes())?;
                self.out.write_all(jpeg)?;
                self.seen.insert(key, self.pos + RECORD_HEADER);
                self.pos += RECORD_HEADER + len as u64;
                self.unique_frames += 1;
            }
        }
        self.frames += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        Ok(self.out.flush()?)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FrameEntry {
    pub at: f64,
    offset: u64,
    len: u32,
}

/// Random access over a frame file. Building the index reads only record headers.
pub struct FrameIndex {
    file: File,
    pub entries: Vec<FrameEntry>,
}

impl FrameIndex {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let total = file.metadata()?.len();
        let mut r = BufReader::new(&mut file);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        // v1 files are v2 files without references.
        if &magic != MAGIC && &magic != b"TSTDFRM1" {
            bail!("{} is not a Testudo frame file", path.display());
        }
        let mut payloads: HashMap<u64, u32> = HashMap::new();

        let mut entries = Vec::new();
        let mut pos = MAGIC.len() as u64;
        let mut head = [0u8; RECORD_HEADER as usize];
        while pos + RECORD_HEADER <= total {
            r.read_exact(&mut head)?;
            let at = f64::from_le_bytes(head[..8].try_into().unwrap());
            let raw_len = u32::from_le_bytes(head[8..].try_into().unwrap());
            let record_len = (raw_len & !REF_FLAG) as u64;
            let body = pos + RECORD_HEADER;
            if body + record_len > total {
                break; // torn write
            }
            if raw_len & REF_FLAG != 0 {
                let mut target = [0u8; 8];
                r.read_exact(&mut target)?;
                let offset = u64::from_le_bytes(target);
                let Some(&len) = payloads.get(&offset) else {
                    bail!("{}: frame reference to unknown offset {offset}", path.display());
                };
                entries.push(FrameEntry { at, offset, len });
            } else {
                payloads.insert(body, raw_len);
                entries.push(FrameEntry { at, offset: body, len: raw_len });
                r.seek_relative(record_len as i64)?;
            }
            pos = body + record_len;
        }
        Ok(Self { file, entries })
    }

    /// Index of the last frame received at or before `at`.
    pub fn index_at(&self, at: f64) -> Option<usize> {
        let n = self.entries.partition_point(|e| e.at <= at);
        n.checked_sub(1)
    }

    pub fn read(&mut self, i: usize) -> Result<Vec<u8>> {
        let e = self.entries[i];
        self.file.seek(SeekFrom::Start(e.offset))?;
        let mut buf = vec![0u8; e.len as usize];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_index_read_and_torn_tail() {
        let dir = std::env::temp_dir().join(format!("testudo-frames-{}", std::process::id()));
        let path = dir.join("p.tfr");
        let mut w = FrameWriter::create(&path).unwrap();
        w.append(10.0, b"aaa").unwrap();
        w.append(20.0, b"bbbb").unwrap();
        w.append(30.0, b"c").unwrap();
        w.flush().unwrap();
        drop(w);

        // simulate a crash mid-record
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&40.0f64.to_le_bytes()).unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap();
        f.write_all(b"partial").unwrap();

        let mut idx = FrameIndex::open(&path).unwrap();
        assert_eq!(idx.entries.len(), 3);
        assert_eq!(idx.index_at(5.0), None);
        assert_eq!(idx.index_at(20.0), Some(1));
        assert_eq!(idx.index_at(25.0), Some(1));
        assert_eq!(idx.index_at(99.0), Some(2));
        assert_eq!(idx.read(1).unwrap(), b"bbbb");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repeated_frames_are_stored_once() {
        let dir = std::env::temp_dir().join(format!("testudo-frames-dedup-{}", std::process::id()));
        let path = dir.join("p.tfr");
        let a = vec![1u8; 5000];
        let b = vec![2u8; 5000];
        let mut w = FrameWriter::create(&path).unwrap();
        for (i, f) in [&a, &b, &a, &b, &a].iter().enumerate() {
            w.append(i as f64, f).unwrap();
        }
        w.flush().unwrap();
        assert_eq!((w.frames, w.unique_frames), (5, 2));
        drop(w);

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < 2 * 5000 + 5 * 20 + 100, "repeats were not deduplicated: {size} bytes");

        let mut idx = FrameIndex::open(&path).unwrap();
        assert_eq!(idx.entries.len(), 5);
        assert_eq!(idx.read(4).unwrap(), a);
        assert_eq!(idx.read(3).unwrap(), b);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
