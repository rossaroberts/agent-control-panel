//! The byte-level half of following a log file: where the last read stopped, the bytes of a line whose newline has not
//! arrived, a first read that begins mid-file, and the reopen when the file shrank or was replaced. `Tail` (Claude
//! transcripts) and `RolloutTail` (Codex rollouts) each put their own line scan on top.

use memchr::memchr_iter;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

fn ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn stat_of(m: &fs::Metadata) -> (u64, u64, i128) {
    (
        m.ino(),
        m.len(),
        m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
    )
}

fn read_at(f: &mut File, from: u64, len: u64) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

#[derive(Debug)]
pub(crate) struct LineReader {
    path: PathBuf,
    offset: u64,
    inode: u64,
    /// The bytes after the last newline, held until theirs arrives.
    partial: Vec<u8>,
    /// The first read began mid-file and has not yet passed the end of the line it began in.
    skip_line: bool,
    pub(crate) written_at: Option<i64>,
    pub(crate) bytes_read: u64,
    /// The last bytes (at most `FINGERPRINT`) before `offset`, as read: a file rewritten in place past them since is
    /// caught by `verify` (Ruling R11).
    fingerprint: Vec<u8>,
    /// The file's (inode, length, mtime ns) as stat'ed at the start of the last read: the reconcile skips the fingerprint
    /// check of a file whose stat has not moved since.
    stat: (u64, u64, i128),
}

/// How many bytes before the offset `verify` compares.
const FINGERPRINT: usize = 64;

/// What a first read took.
pub(crate) struct First {
    /// The first `head` bytes, when the tail window did not start at the beginning (else empty: the body holds them).
    pub head: Vec<u8>,
    /// From the tail window's start to the end of the file.
    pub body: Vec<u8>,
    /// The tail window starts at byte 0.
    pub whole: bool,
}

/// What `advance` found.
pub(crate) enum Step {
    /// Shrunk or replaced: the owner reads again from scratch.
    Reopen,
    Appended(Vec<u8>),
    Unchanged,
}

impl LineReader {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The first read: the last `tail` bytes (the line it starts inside dropped) and, when that is not the whole file,
    /// the first `head` bytes: at most `tail + head`, and never more than the file.
    pub(crate) fn open(path: &Path, tail: u64, head: u64) -> io::Result<(LineReader, First)> {
        let mut f = File::open(path)?;
        let meta = f.metadata()?;
        let size = meta.len();
        let start = size.saturating_sub(tail);
        let mut r = LineReader {
            path: path.to_path_buf(),
            offset: start,
            inode: meta.ino(),
            partial: Vec::new(),
            skip_line: start > 0,
            written_at: meta.modified().ok().and_then(ms),
            bytes_read: 0,
            fingerprint: Vec::new(),
            stat: stat_of(&meta),
        };
        // The head and the tail overlap in a file under tail + head: then one read of it all.
        let (head_bytes, body) = if start > 0 && start < head {
            let mut all = read_at(&mut f, 0, size)?;
            r.bytes_read += all.len() as u64;
            let body = all.get(start as usize..).unwrap_or_default().to_vec();
            all.truncate(head as usize);
            (all, body)
        } else {
            let h = if start > 0 {
                read_at(&mut f, 0, head)?
            } else {
                Vec::new()
            };
            let body = read_at(&mut f, start, size - start)?;
            r.bytes_read += (h.len() + body.len()) as u64;
            (h, body)
        };
        Ok((
            r,
            First {
                head: head_bytes,
                body,
                whole: start == 0,
            },
        ))
    }

    /// Reads what was appended since the last read, and only that.
    pub(crate) fn advance(&mut self) -> io::Result<Step> {
        let meta = fs::metadata(&self.path)?;
        let written_at = meta.modified().ok().and_then(ms);
        if meta.ino() != self.inode || meta.len() < self.offset {
            return Ok(Step::Reopen);
        }
        self.written_at = written_at;
        self.stat = stat_of(&meta);
        if meta.len() > self.offset {
            let mut f = File::open(&self.path)?;
            let new = read_at(&mut f, self.offset, meta.len() - self.offset)?;
            self.bytes_read += new.len() as u64;
            Ok(Step::Appended(new))
        } else {
            Ok(Step::Unchanged)
        }
    }

    /// Whether the file's stat (inode, length, mtime) differs from the last read's. An error when it cannot be stat'ed.
    pub(crate) fn moved(&self) -> io::Result<bool> {
        Ok(stat_of(&fs::metadata(&self.path)?) != self.stat)
    }

    /// Whether the file still holds, just before the offset, the bytes read there: false when it was replaced, shrank, or
    /// was rewritten in place (same inode, same or greater length), which `advance` alone cannot see. The owner then reads
    /// it again from scratch.
    pub(crate) fn verify(&self) -> io::Result<bool> {
        let meta = fs::metadata(&self.path)?;
        if meta.ino() != self.inode || meta.len() < self.offset {
            return Ok(false);
        }
        let n = self.fingerprint.len() as u64;
        if n == 0 {
            return Ok(true);
        }
        let mut f = File::open(&self.path)?;
        Ok(read_at(&mut f, self.offset - n, n)? == self.fingerprint)
    }

    /// Takes in bytes read at the offset: calls `each(line, end)` for every line they complete, `end` the file offset
    /// just past its newline. Whether any non-empty line was taken in.
    pub(crate) fn feed(&mut self, bytes: &[u8], mut each: impl FnMut(&[u8], u64)) -> bool {
        let base = self.offset - self.partial.len() as u64; // where `data` starts in the file
        self.offset += bytes.len() as u64;
        if bytes.len() >= FINGERPRINT {
            self.fingerprint = bytes[bytes.len() - FINGERPRINT..].to_vec();
        } else {
            self.fingerprint.extend_from_slice(bytes);
            let extra = self.fingerprint.len().saturating_sub(FINGERPRINT);
            self.fingerprint.drain(..extra);
        }
        let held = std::mem::take(&mut self.partial);
        let data: &[u8] = if held.is_empty() {
            bytes
        } else {
            &[held.as_slice(), bytes].concat()
        };
        let mut from = 0;
        let mut took = false;
        for nl in memchr_iter(b'\n', data) {
            let l = &data[from..nl];
            from = nl + 1;
            if std::mem::take(&mut self.skip_line) {
                continue;
            }
            if !l.is_empty() {
                each(l, base + from as u64);
                took = true;
            }
        }
        if !self.skip_line {
            self.partial = data[from..].to_vec();
        }
        took
    }
}
