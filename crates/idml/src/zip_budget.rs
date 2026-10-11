//! Bound ZIP index allocation before handing metadata to `zip`.
//! Record fields follow PKWARE APPNOTE 6.3.10, sections 4.3.14–4.3.16:
//! https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT

use std::cell::Cell;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::rc::Rc;

use crate::{IdmlError, Result};

fn invalid() -> IdmlError {
    IdmlError::NotIdml("invalid or unsupported IDML ZIP directory metadata".into())
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    offset.checked_add(N).and_then(|end| bytes.get(offset..end)).and_then(|value| value.try_into().ok()).ok_or_else(invalid)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(field(bytes, offset)?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(field(bytes, offset)?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(field(bytes, offset)?))
}

struct Directory {
    entries: u64,
    start: usize,
    footer: usize,
}

fn check_entries(entries: u64, max_entries: usize) -> Result<()> {
    if entries > u64::try_from(max_entries).map_err(|_| invalid())? {
        return Err(IdmlError::NotIdml(format!("package exceeds the {max_entries}-entry import budget")));
    }
    Ok(())
}

fn directory(bytes: &[u8], max_entries: usize) -> Result<Directory> {
    let tail_start = bytes.len().saturating_sub(22 + usize::from(u16::MAX));
    let tail = bytes.get(tail_start..).ok_or_else(invalid)?;
    let footer = tail
        .windows(22)
        .enumerate()
        .rev()
        .find_map(|(offset, record)| {
            if record.get(..4) != Some(b"PK\x05\x06".as_slice()) {
                return None;
            }
            let comment = usize::from(u16_at(record, 20).ok()?);
            let position = tail_start.checked_add(offset)?;
            (position.checked_add(22)?.checked_add(comment)? == bytes.len()).then_some(position)
        })
        .ok_or_else(invalid)?;
    let record = bytes.get(footer..).ok_or_else(invalid)?;
    let classic_disk_entries = u16_at(record, 8)?;
    let classic_entries = u16_at(record, 10)?;
    let classic_size = u32_at(record, 12)?;
    let classic_offset = u32_at(record, 16)?;
    if u16_at(record, 4)? != 0 || u16_at(record, 6)? != 0 {
        return Err(invalid());
    }
    for count in [classic_disk_entries, classic_entries] {
        if count != u16::MAX {
            check_entries(u64::from(count), max_entries)?;
        }
    }
    let locator = footer.checked_sub(20).and_then(|start| bytes.get(start..footer));
    let (entries, size, offset, metadata_end) = if let Some(locator) = locator.filter(|value| value.get(..4) == Some(b"PK\x06\x07".as_slice())) {
        if u32_at(locator, 4)? != 0 || u32_at(locator, 16)? != 1 {
            return Err(invalid()); // Only single-disk packages are supported.
        }
        let start = usize::try_from(u64_at(locator, 8)?).map_err(|_| invalid())?;
        let zip64 = bytes.get(start..).ok_or_else(invalid)?;
        if zip64.get(..4) != Some(b"PK\x06\x06".as_slice()) {
            return Err(invalid());
        }
        let length = u64_at(zip64, 4)?;
        let end = u64::try_from(start).ok().and_then(|start| start.checked_add(12)?.checked_add(length)).ok_or_else(invalid)?;
        if length < 44 || end != u64::try_from(footer.saturating_sub(20)).map_err(|_| invalid())? {
            return Err(invalid());
        }
        if u16_at(zip64, 14)? > u16_at(zip64, 12)? || u32_at(zip64, 16)? != 0 || u32_at(zip64, 20)? != 0 {
            return Err(invalid());
        }
        let entries = u64_at(zip64, 32)?;
        if u64_at(zip64, 24)? != entries {
            return Err(invalid());
        }
        let size = u64_at(zip64, 40)?;
        let offset = u64_at(zip64, 48)?;
        if [classic_disk_entries, classic_entries].into_iter().any(|count| count != u16::MAX && u64::from(count) != entries)
            || (classic_size != u32::MAX && u64::from(classic_size) != size)
            || (classic_offset != u32::MAX && u64::from(classic_offset) != offset)
            // zip 3 selects the ZIP64 footer through the total count or directory offset.
            || (classic_disk_entries == u16::MAX && classic_entries != u16::MAX && classic_offset != u32::MAX)
        {
            return Err(invalid());
        }
        (entries, size, offset, start)
    } else {
        if classic_disk_entries != classic_entries || classic_entries == u16::MAX || classic_size == u32::MAX || classic_offset == u32::MAX {
            return Err(invalid());
        }
        (u64::from(classic_entries), u64::from(classic_size), u64::from(classic_offset), footer)
    };
    check_entries(entries, max_entries)?;
    let end = offset.checked_add(size).ok_or_else(invalid)?;
    if end > u64::try_from(metadata_end).map_err(|_| invalid())? || size < entries.checked_mul(46).ok_or_else(invalid)? {
        return Err(invalid());
    }
    Ok(Directory { entries, start: usize::try_from(offset).map_err(|_| invalid())?, footer })
}

/// During index construction, only the selected footer and one directory pass are accepted.
/// This prevents `zip` from retrying an older, unvalidated footer after malformed metadata.
pub(super) struct BudgetReader<'a> {
    inner: Cursor<&'a [u8]>,
    directory_start: u64,
    comment_length: u64,
    indexing: Rc<Cell<bool>>,
    index_started: bool,
    probe_rewound: bool,
    nonempty: bool,
}

impl Read for BudgetReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let position = self.inner.position();
        let length = self.inner.read(out)?;
        let output = out.get_mut(..length).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        // The importer does not consume archive comments. Hide them from footer discovery so a
        // signature inside a comment cannot substitute another entry count before indexing.
        if self.indexing.get() {
            for offset in [self.comment_length, self.comment_length.saturating_add(1)] {
                if let Some(local) = offset.checked_sub(position).and_then(|local| usize::try_from(local).ok())
                    && let Some(byte) = output.get_mut(local)
                {
                    *byte = 0;
                }
            }
        }
        Ok(length)
    }
}

impl Seek for BudgetReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let unchanged = matches!(position, SeekFrom::Current(0)) || matches!(position, SeekFrom::Start(offset) if offset == self.inner.position());
        if self.indexing.get() && self.index_started && !unchanged {
            // `zip` probes the four-byte directory signature before reading its entries.
            // Permit that one rewind, then require a forward index pass without recovery.
            let probe = !self.probe_rewound
                && self.directory_start.checked_add(4) == Some(self.inner.position())
                && matches!(position, SeekFrom::Start(offset) if offset == self.directory_start);
            if !probe {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "IDML ZIP directory recovery is disabled"));
            }
            self.probe_rewound = true;
        }
        let target = self.inner.seek(position)?;
        if self.indexing.get() && self.nonempty && matches!(position, SeekFrom::Start(_)) && target == self.directory_start {
            self.index_started = true;
        }
        Ok(target)
    }
}

pub(super) fn open(bytes: &[u8], max_entries: usize) -> Result<zip::ZipArchive<BudgetReader<'_>>> {
    let directory = directory(bytes, max_entries)?;
    let indexing = Rc::new(Cell::new(true));
    let end = directory.footer.checked_add(22).ok_or_else(invalid)?;
    let reader = BudgetReader {
        inner: Cursor::new(bytes.get(..end).ok_or_else(invalid)?),
        directory_start: u64::try_from(directory.start).map_err(|_| invalid())?,
        comment_length: u64::try_from(directory.footer.checked_add(20).ok_or_else(invalid)?).map_err(|_| invalid())?,
        indexing: indexing.clone(),
        index_started: false,
        probe_rewound: false,
        nonempty: directory.entries != 0,
    };
    let config = zip::read::Config { archive_offset: zip::read::ArchiveOffset::Known(0) };
    let archive = zip::ZipArchive::with_config(config, reader).map_err(|error| IdmlError::NotIdml(error.to_string()))?;
    indexing.set(false);
    Ok(archive)
}
