//! Extended-attribute transfer through the Win32 backup stream protocol.
//!
//! The public type contains only the opaque `BACKUP_EA_DATA` payload. Source
//! and destination handles are synchronous and buffered, as required by
//! `BackupRead` and `BackupWrite`; they are intentionally separate from any
//! future overlapped/no-buffering data handles.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    BACKUP_EA_DATA, BackupRead, BackupSeek, BackupWrite, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::util::{bool_result, last_error};

const STREAM_HEADER_BYTES: usize = 20;
const MAX_EA_PAYLOAD_BYTES: u64 = 1024 * 1024;

/// An opaque, validated `BACKUP_EA_DATA` payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtendedAttributes(Vec<u8>);

impl ExtendedAttributes {
    /// Builds a validated EA set from raw name/value byte pairs.
    ///
    /// EA names are byte strings in the Win32 API. They must be non-empty,
    /// contain no NUL, and fit the on-disk one-byte length field; values use a
    /// two-byte length field. This constructor is also used by the bounded
    /// testkit without exposing the internal backup-stream framing.
    pub fn from_pairs(entries: &[(&[u8], &[u8])]) -> io::Result<Self> {
        let mut records = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            if name.is_empty() || name.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "EA names must be non-empty and contain no NUL",
                ));
            }
            let name_length = u8::try_from(name.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "EA name exceeds 255 bytes")
            })?;
            let value_length = u16::try_from(value.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "EA value exceeds 65535 bytes")
            })?;
            let raw_length = 8_usize
                .checked_add(name.len())
                .and_then(|length| length.checked_add(1))
                .and_then(|length| length.checked_add(value.len()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "EA size overflow"))?;
            records.push((name, value, name_length, value_length, raw_length));
        }

        let mut payload = Vec::new();
        for (index, (name, value, name_length, value_length, raw_length)) in
            records.iter().enumerate()
        {
            let padded = raw_length.next_multiple_of(4);
            let next = if index + 1 == records.len() {
                0
            } else {
                padded
            };
            payload.extend_from_slice(
                &u32::try_from(next)
                    .map_err(|_| io::Error::other("EA record offset exceeds u32"))?
                    .to_le_bytes(),
            );
            payload.push(0);
            payload.push(*name_length);
            payload.extend_from_slice(&value_length.to_le_bytes());
            payload.extend_from_slice(name);
            payload.push(0);
            payload.extend_from_slice(value);
            payload.resize(payload.len() + padded - raw_length, 0);
        }
        if payload.len() as u64 > MAX_EA_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "EA set exceeds the one MiB safety limit",
            ));
        }
        Ok(Self(payload))
    }

    /// Returns true when the source had no extended attributes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the raw EA payload for independent test or diagnostic code.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Reads only the EA backup stream from a file, directory, or reparse point.
pub fn read_extended_attributes(path: &Path) -> io::Result<ExtendedAttributes> {
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    read_from_file(&file)
}

/// Writes an EA payload to an existing file, directory, or reparse point.
///
/// Windows treats an empty `BACKUP_EA_DATA` stream as a no-op. Call
/// [`clear_extended_attributes`] first when the destination may contain names
/// absent from the source snapshot.
pub fn write_extended_attributes(path: &Path, attributes: &ExtendedAttributes) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    write_to_file(&file, attributes)
}

/// Deletes every EA currently attached to an object.
///
/// Windows deletes an EA when a `FILE_FULL_EA_INFORMATION` record names it
/// with a zero-length value. An empty backup stream alone is a no-op, so
/// directory reconciliation uses this explicit operation before writing the
/// source set.
pub fn clear_extended_attributes(path: &Path) -> io::Result<()> {
    let existing = read_extended_attributes(path)?;
    if existing.is_empty() {
        return Ok(());
    }
    let tombstones = ExtendedAttributes(ea_tombstones(existing.as_bytes())?);
    write_extended_attributes(path, &tombstones)
}

fn ea_tombstones(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut names = Vec::new();
    let mut offset = 0_usize;
    loop {
        if offset.checked_add(8).is_none_or(|end| end > payload.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated FILE_FULL_EA_INFORMATION header",
            ));
        }
        let next = u32::from_le_bytes(array(&payload[offset..offset + 4], "EA next offset")?);
        let flags = payload[offset + 4];
        let name_length = usize::from(payload[offset + 5]);
        let name_start = offset + 8;
        let name_end = name_start
            .checked_add(name_length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "EA name overflow"))?;
        if name_end >= payload.len() || payload[name_end] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated or unterminated EA name",
            ));
        }
        names.push((flags, payload[name_start..name_end].to_vec()));
        if next == 0 {
            break;
        }
        let next = usize::try_from(next)
            .map_err(|_| io::Error::other("EA next offset exceeds address space"))?;
        if next < 8
            || offset
                .checked_add(next)
                .is_none_or(|value| value >= payload.len())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid EA next offset",
            ));
        }
        offset += next;
    }

    let mut result = Vec::new();
    let count = names.len();
    for (index, (flags, name)) in names.into_iter().enumerate() {
        let raw_length = 8_usize
            .checked_add(name.len())
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "EA record overflow"))?;
        let padded = raw_length.next_multiple_of(4);
        let next = if index + 1 == count { 0 } else { padded };
        result.extend_from_slice(
            &u32::try_from(next)
                .map_err(|_| io::Error::other("EA record offset exceeds u32"))?
                .to_le_bytes(),
        );
        result.push(flags);
        result.push(
            u8::try_from(name.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "EA name exceeds u8"))?,
        );
        result.extend_from_slice(&0_u16.to_le_bytes());
        result.extend_from_slice(&name);
        result.push(0);
        result.resize(result.len() + padded - raw_length, 0);
    }
    Ok(result)
}

pub(crate) fn read_from_file(file: &File) -> io::Result<ExtendedAttributes> {
    let mut reader = BackupReader::new(file);
    loop {
        let Some(header) = reader.read_header()? else {
            return Ok(ExtendedAttributes(Vec::new()));
        };
        let stream_id = u32::from_le_bytes(array(&header[0..4], "stream identifier")?);
        let size_signed = i64::from_le_bytes(array(&header[8..16], "stream size")?);
        let size = u64::try_from(size_signed).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "negative backup stream size")
        })?;
        let name_bytes = u32::from_le_bytes(array(&header[16..20], "stream name size")?);
        reader.skip(u64::from(name_bytes))?;
        if stream_id == BACKUP_EA_DATA {
            if size > MAX_EA_PAYLOAD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("EA backup payload is unexpectedly large: {size} bytes"),
                ));
            }
            let length = usize::try_from(size)
                .map_err(|_| io::Error::other("EA payload does not fit address space"))?;
            let mut payload = vec![0_u8; length];
            reader.read_exact(&mut payload)?;
            return Ok(ExtendedAttributes(payload));
        }
        reader.skip(size)?;
    }
}

pub(crate) fn write_to_file(file: &File, attributes: &ExtendedAttributes) -> io::Result<()> {
    let size = i64::try_from(attributes.0.len())
        .map_err(|_| io::Error::other("EA payload does not fit backup stream header"))?;
    let mut header = [0_u8; STREAM_HEADER_BYTES];
    header[0..4].copy_from_slice(&BACKUP_EA_DATA.to_le_bytes());
    header[8..16].copy_from_slice(&size.to_le_bytes());
    let mut writer = BackupWriter::new(file);
    writer.write_all(&header)?;
    writer.write_all(&attributes.0)
}

fn array<const N: usize>(bytes: &[u8], field: &str) -> io::Result<[u8; N]> {
    bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid backup {field}"),
        )
    })
}

struct BackupReader<'a> {
    file: &'a File,
    context: *mut c_void,
}

impl<'a> BackupReader<'a> {
    const fn new(file: &'a File) -> Self {
        Self {
            file,
            context: ptr::null_mut(),
        }
    }

    fn read_header(&mut self) -> io::Result<Option<[u8; STREAM_HEADER_BYTES]>> {
        let mut header = [0_u8; STREAM_HEADER_BYTES];
        let first = self.read_some(&mut header)?;
        if first == 0 {
            return Ok(None);
        }
        self.read_exact(&mut header[first..])?;
        Ok(Some(header))
    }

    fn read_exact(&mut self, mut output: &mut [u8]) -> io::Result<()> {
        while !output.is_empty() {
            let read = self.read_some(output)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backup stream ended unexpectedly",
                ));
            }
            output = &mut output[read..];
        }
        Ok(())
    }

    fn read_some(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let requested = u32::try_from(output.len())
            .map_err(|_| io::Error::other("backup read request exceeds u32"))?;
        let mut read = 0_u32;
        // SAFETY: output is writable for requested bytes, context is owned by
        // this reader, and the borrowed file handle remains valid.
        let succeeded = unsafe {
            BackupRead(
                self.file.as_raw_handle().cast(),
                output.as_mut_ptr(),
                requested,
                &raw mut read,
                0,
                0,
                &raw mut self.context,
            )
        };
        if succeeded == 0 {
            let error = last_error();
            // BackupRead uses FALSE + ERROR_SUCCESS to signal the clean end
            // of an object that exposes no more backup streams.
            if error.raw_os_error() == Some(0) {
                return Ok(0);
            }
            return Err(error);
        }
        usize::try_from(read).map_err(|_| io::Error::other("backup read count is too large"))
    }

    fn skip(&mut self, mut bytes: u64) -> io::Result<()> {
        while bytes > 0 {
            let requested = bytes.min(u64::from(u32::MAX));
            let requested_u32 = u32::try_from(requested)
                .map_err(|_| io::Error::other("backup seek request exceeds u32"))?;
            let mut low = 0_u32;
            let mut high = 0_u32;
            // SAFETY: output counters and context are valid, and the borrowed
            // file handle remains open.
            let succeeded = unsafe {
                BackupSeek(
                    self.file.as_raw_handle().cast(),
                    requested_u32,
                    0,
                    &raw mut low,
                    &raw mut high,
                    &raw mut self.context,
                )
            };
            let skipped = u64::from(low) | (u64::from(high) << 32);
            if succeeded == 0 {
                let error = last_error();
                // Some filesystem drivers return FALSE/ERROR_SUCCESS after a
                // successful end-of-stream seek while still reporting the
                // exact skipped byte count. Others decline BackupSeek with
                // FALSE/ERROR_SUCCESS and zero progress; fall back to a
                // bounded read-and-discard loop in that case.
                if error.raw_os_error() == Some(0) && skipped == 0 {
                    return self.discard(bytes);
                }
                if error.raw_os_error() != Some(0) || skipped != requested {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "BackupSeek failed: {error}; requested={requested}, skipped={skipped}"
                        ),
                    ));
                }
            }
            if skipped == 0 || skipped > bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "could not skip backup stream payload",
                ));
            }
            bytes -= skipped;
        }
        Ok(())
    }

    fn discard(&mut self, mut bytes: u64) -> io::Result<()> {
        let mut buffer = vec![0_u8; 64 * 1024];
        while bytes > 0 {
            let requested = usize::try_from(bytes.min(buffer.len() as u64))
                .map_err(|_| io::Error::other("backup discard request exceeds address space"))?;
            let read = self.read_some(&mut buffer[..requested])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backup stream ended while discarding a payload",
                ));
            }
            bytes -= read as u64;
        }
        Ok(())
    }
}

impl Drop for BackupReader<'_> {
    fn drop(&mut self) {
        let mut ignored = 0_u32;
        // SAFETY: bAbort releases this reader's context. No data buffer is
        // accessed when the requested size is zero.
        unsafe {
            let _ = BackupRead(
                self.file.as_raw_handle().cast(),
                ptr::null_mut(),
                0,
                &raw mut ignored,
                1,
                0,
                &raw mut self.context,
            );
        }
    }
}

struct BackupWriter<'a> {
    file: &'a File,
    context: *mut c_void,
}

impl<'a> BackupWriter<'a> {
    const fn new(file: &'a File) -> Self {
        Self {
            file,
            context: ptr::null_mut(),
        }
    }

    fn write_all(&mut self, mut input: &[u8]) -> io::Result<()> {
        while !input.is_empty() {
            let requested = u32::try_from(input.len())
                .map_err(|_| io::Error::other("backup write request exceeds u32"))?;
            let mut written = 0_u32;
            // SAFETY: input is readable for requested bytes, context is owned
            // by this writer, and the borrowed file handle remains valid.
            unsafe {
                bool_result(BackupWrite(
                    self.file.as_raw_handle().cast(),
                    input.as_ptr(),
                    requested,
                    &raw mut written,
                    0,
                    0,
                    &raw mut self.context,
                ))?;
            }
            let written = usize::try_from(written)
                .map_err(|_| io::Error::other("backup write count is too large"))?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "backup writer made no progress",
                ));
            }
            input = &input[written..];
        }
        Ok(())
    }
}

impl Drop for BackupWriter<'_> {
    fn drop(&mut self) {
        let mut ignored = 0_u32;
        // SAFETY: bAbort releases this writer's context. No data buffer is
        // accessed when the requested size is zero.
        unsafe {
            let _ = BackupWrite(
                self.file.as_raw_handle().cast(),
                ptr::null(),
                0,
                &raw mut ignored,
                1,
                0,
                &raw mut self.context,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtendedAttributes, read_extended_attributes, write_extended_attributes};
    use std::fs;

    #[test]
    fn backup_ea_round_trip_stays_in_temp_sandbox() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let path = sandbox.path().join("ea.bin");
        assert!(fs::write(&path, b"ordinary data").is_ok());

        let requested = ExtendedAttributes::from_pairs(&[(b"bigcp.e", b"value")]);
        assert!(requested.is_ok());
        let Some(requested) = requested.ok() else {
            return;
        };
        assert!(write_extended_attributes(&path, &requested).is_ok());
        let canonical = read_extended_attributes(&path);
        assert!(canonical.is_ok());
        // NTFS stores EA names case-insensitively and canonicalizes their
        // spelling. Copy fidelity is therefore defined by the bytes returned
        // by BackupRead, not by the caller's pre-write spelling.
        let Some(canonical) = canonical.ok() else {
            return;
        };
        assert!(write_extended_attributes(&path, &canonical).is_ok());
        assert_eq!(read_extended_attributes(&path).ok(), Some(canonical));
        assert!(super::clear_extended_attributes(&path).is_ok());
        assert_eq!(
            read_extended_attributes(&path).ok(),
            Some(ExtendedAttributes(Vec::new()))
        );
        assert_eq!(fs::read(path).ok(), Some(b"ordinary data".to_vec()));
    }
}
