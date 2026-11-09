use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd};

const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;
const FIEMAP_FLAG_SYNC: u32 = 0x0000_0001;
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
const FIEMAP_EXTENT_DATA_ENCRYPTED: u32 = 0x0000_0080;
const FIEMAP_EXTENT_NOT_ALIGNED: u32 = 0x0000_0100;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x0000_0400;
const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0800;
const FILE_DEDUPE_RANGE_SAME: i32 = 0;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
const MAX_DEDUPE_CHUNK: u64 = 16 * 1024 * 1024;
const FIEMAP_BATCH_SIZE: usize = 128;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapHeader {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FileDedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    info: FileDedupeRangeInfo,
}

#[repr(C)]
struct FileDedupeRangeInfo {
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub logical: u64,
    physical: u64,
    pub length: u64,
    flags: u32,
}

#[derive(Default)]
pub struct DedupeSummary {
    pub bytes_deduped: u64,
    pub skipped_extents: usize,
    pub skipped_bytes: u64,
    pub differing_ranges: usize,
    pub failed_ranges: usize,
}

impl DedupeSummary {
    pub fn is_incomplete(&self) -> bool {
        self.failed_ranges > 0 || self.skipped_bytes > 0
    }
}

pub fn open_unnamed_temp(directory: &File) -> io::Result<File> {
    let name = b".\0";
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr().cast(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("could not create O_TMPFILE in target directory: {error}"),
        ));
    }

    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn fiemap(file: &File) -> io::Result<Vec<Extent>> {
    let mut start = 0_u64;
    let mut extents = Vec::new();

    loop {
        let words = (size_of::<FiemapHeader>() + FIEMAP_BATCH_SIZE * size_of::<FiemapExtent>())
            / size_of::<u64>();
        let mut storage = vec![0_u64; words];
        let header = storage.as_mut_ptr().cast::<FiemapHeader>();
        unsafe {
            (*header).start = start;
            (*header).length = u64::MAX - start;
            (*header).flags = FIEMAP_FLAG_SYNC;
            (*header).extent_count = FIEMAP_BATCH_SIZE as u32;
        }

        let result = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, header) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        let mapped = unsafe { (*header).mapped_extents as usize };
        if mapped > FIEMAP_BATCH_SIZE {
            return Err(io::Error::other(
                "FIEMAP returned more extents than requested",
            ));
        }
        if mapped == 0 {
            break;
        }

        let extent_ptr = unsafe {
            storage
                .as_ptr()
                .add(size_of::<FiemapHeader>() / size_of::<u64>())
        }
        .cast::<FiemapExtent>();
        let mut last_flag = false;
        for index in 0..mapped {
            let extent = unsafe { *extent_ptr.add(index) };
            extents.push(Extent {
                logical: extent.logical,
                physical: extent.physical,
                length: extent.length,
                flags: extent.flags,
            });
            last_flag |= extent.flags & FIEMAP_EXTENT_LAST != 0;
        }

        if last_flag || mapped < FIEMAP_BATCH_SIZE {
            break;
        }

        let last = unsafe { *extent_ptr.add(mapped - 1) };
        let next = last
            .logical
            .checked_add(last.length)
            .ok_or_else(|| io::Error::other("FIEMAP extent offset overflow"))?;
        if next <= start || next == u64::MAX {
            return Err(io::Error::other("FIEMAP pagination did not advance"));
        }
        start = next;
    }

    Ok(extents)
}

pub fn filefrag_extent_count(extents: &[Extent]) -> usize {
    let mut count = 0;
    let mut previous: Option<&Extent> = None;
    let unknown_flags = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC;

    for extent in extents {
        let can_merge = previous.is_some_and(|previous| {
            previous.flags & unknown_flags == 0
                && extent.flags & unknown_flags == 0
                && previous.logical.checked_add(previous.length) == Some(extent.logical)
                && previous.physical.checked_add(previous.length) == Some(extent.physical)
        });
        if !can_merge {
            count += 1;
        }
        previous = Some(extent);
    }

    count
}

pub fn debug_extent_map(label: &str, extents: &[Extent]) {
    eprintln!(
        "{label}: {} FIEMAP records, {} coalesced extents",
        extents.len(),
        filefrag_extent_count(extents)
    );
    for (index, extent) in extents.iter().enumerate() {
        eprintln!(
            "{label}[{index}]: logical={} physical={} length={} flags={:#x}",
            extent.logical, extent.physical, extent.length, extent.flags
        );
    }
}

pub fn filesystem_block_size(file: &File) -> io::Result<u64> {
    let mut stats = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::fstatvfs(file.as_raw_fd(), &mut stats) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = stats.f_bsize as u64;
    if size == 0 {
        return Err(io::Error::other("filesystem reported a zero block size"));
    }
    Ok(size)
}

pub fn copy_contents(source: &mut File, target: &mut File, size: u64) -> io::Result<bool> {
    match copy_sparse(source, target, size) {
        Ok(()) => Ok(true),
        Err(error) if is_seek_hole_unsupported(&error) => {
            target.set_len(0)?;
            copy_region(source, target, 0, size)?;
            target.set_len(size)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn copy_sparse(source: &mut File, target: &mut File, size: u64) -> io::Result<()> {
    let mut offset = 0_u64;
    while offset < size {
        let data = match seek_data(source, offset) {
            Ok(data) => data,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => break,
            Err(error) => return Err(error),
        };
        if data >= size {
            break;
        }
        let hole = match seek_hole(source, data) {
            Ok(hole) => hole,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => size,
            Err(error) => return Err(error),
        };
        if hole <= data {
            return Err(io::Error::other(
                "SEEK_HOLE returned a non-advancing offset",
            ));
        }
        let end = hole.min(size);
        copy_region(source, target, data, end)?;
        offset = end;
    }
    target.set_len(size)
}

fn seek_data(file: &File, offset: u64) -> io::Result<u64> {
    seek_extent(file, offset, libc::SEEK_DATA)
}

fn seek_hole(file: &File, offset: u64) -> io::Result<u64> {
    seek_extent(file, offset, libc::SEEK_HOLE)
}

fn seek_extent(file: &File, offset: u64, whence: libc::c_int) -> io::Result<u64> {
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset is too large"))?;
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as u64)
    }
}

fn is_seek_hole_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOTSUP)
    )
}

fn copy_region(source: &mut File, target: &mut File, start: u64, end: u64) -> io::Result<()> {
    source.seek(SeekFrom::Start(start))?;
    target.seek(SeekFrom::Start(start))?;
    let mut remaining = end - start;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    while remaining > 0 {
        let amount = remaining.min(buffer.len() as u64) as usize;
        let read = source.read(&mut buffer[..amount])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "target became shorter while it was being copied",
            ));
        }
        target.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    Ok(())
}

pub fn dedupe_extents(
    source: &File,
    target: &File,
    extents: &[Extent],
    block_size: u64,
    verbose: bool,
) -> DedupeSummary {
    let mut summary = DedupeSummary::default();
    let mut chunk_limit = MAX_DEDUPE_CHUNK;
    if block_size == 0 {
        summary.failed_ranges = 1;
        eprintln!("cannot deduplicate with a zero filesystem block size");
        return summary;
    }

    let unusable_flags = FIEMAP_EXTENT_UNKNOWN
        | FIEMAP_EXTENT_DELALLOC
        | FIEMAP_EXTENT_DATA_ENCRYPTED
        | FIEMAP_EXTENT_NOT_ALIGNED
        | FIEMAP_EXTENT_DATA_INLINE
        | FIEMAP_EXTENT_DATA_TAIL
        | FIEMAP_EXTENT_UNWRITTEN;

    for extent in extents {
        if extent.flags & unusable_flags != 0 || extent.logical % block_size != 0 {
            if verbose {
                eprintln!(
                    "skipping extent at {} (length {}, flags {:#x}, block size {})",
                    extent.logical, extent.length, extent.flags, block_size
                );
            }
            summary.skipped_extents += 1;
            summary.skipped_bytes += extent.length;
            continue;
        }

        let aligned_length = extent.length - extent.length % block_size;
        if aligned_length == 0 {
            if verbose {
                eprintln!("skipping sub-block extent at {}", extent.logical);
            }
            summary.skipped_extents += 1;
            summary.skipped_bytes += extent.length;
            continue;
        }
        if aligned_length != extent.length {
            let tail = extent.length - aligned_length;
            if verbose {
                eprintln!(
                    "skipping {tail} unaligned tail bytes at {}",
                    extent.logical + aligned_length,
                );
            }
            summary.skipped_bytes += tail;
        }

        let mut offset = extent.logical;
        let Some(end) = extent.logical.checked_add(aligned_length) else {
            summary.failed_ranges += 1;
            eprintln!("extent offset overflow at {}", extent.logical);
            continue;
        };
        while offset < end {
            let Some(requested) = aligned_chunk(end - offset, chunk_limit, block_size) else {
                summary.failed_ranges += 1;
                eprintln!("cannot form an aligned dedupe range at {offset}");
                break;
            };

            match dedupe_range(source, target, offset, requested) {
                Ok((status, bytes)) => {
                    if status == FILE_DEDUPE_RANGE_SAME {
                        if bytes == 0 || bytes > requested || bytes % block_size != 0 {
                            summary.failed_ranges += 1;
                            eprintln!("invalid dedupe byte count {bytes} for range at {offset}");
                            break;
                        }
                        summary.bytes_deduped += bytes;
                        offset += bytes;
                    } else {
                        if status == FILE_DEDUPE_RANGE_DIFFERS {
                            summary.differing_ranges += 1;
                            eprintln!("data differed during dedupe at offset {offset}");
                        } else if status < 0 {
                            summary.failed_ranges += 1;
                            let error = io::Error::from_raw_os_error(-status);
                            eprintln!("dedupe failed at offset {offset}: {error}");
                        } else {
                            summary.failed_ranges += 1;
                            eprintln!("unexpected dedupe status {status} at offset {offset}");
                        }
                        offset += requested;
                    }
                }
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOMEM))
                        && requested > block_size =>
                {
                    chunk_limit = (requested / 2 / block_size) * block_size;
                    if chunk_limit == 0 {
                        chunk_limit = block_size;
                    }
                }
                Err(error) => {
                    summary.failed_ranges += 1;
                    eprintln!("dedupe ioctl failed at offset {offset}: {error}");
                    return summary;
                }
            }
        }
    }

    summary
}

fn aligned_chunk(remaining: u64, limit: u64, block_size: u64) -> Option<u64> {
    if block_size == 0 {
        return None;
    }
    let length = remaining.min(limit);
    let aligned = length - length % block_size;
    (aligned > 0).then_some(aligned)
}

fn dedupe_range(source: &File, target: &File, offset: u64, length: u64) -> io::Result<(i32, u64)> {
    let mut request = FileDedupeRange {
        src_offset: offset,
        src_length: length,
        dest_count: 1,
        reserved1: 0,
        reserved2: 0,
        info: FileDedupeRangeInfo {
            dest_fd: i64::from(target.as_raw_fd()),
            dest_offset: offset,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        },
    };
    let result = unsafe {
        libc::ioctl(
            source.as_raw_fd(),
            FIDEDUPERANGE,
            &mut request as *mut FileDedupeRange,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((request.info.status, request.info.bytes_deduped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_file() -> io::Result<File> {
        let directory = File::open(".")?;
        open_unnamed_temp(&directory)
    }

    #[test]
    fn fiemap_struct_sizes_match_linux_abi() {
        assert_eq!(size_of::<FiemapHeader>(), 32);
        assert_eq!(size_of::<FiemapExtent>(), 56);
        assert_eq!(size_of::<FileDedupeRange>(), 56);
    }

    #[test]
    fn extent_count_merges_contiguous_logical_and_physical_records() {
        let extents = [
            Extent {
                logical: 0,
                physical: 100,
                length: 10,
                flags: 0,
            },
            Extent {
                logical: 10,
                physical: 110,
                length: 10,
                flags: FIEMAP_EXTENT_LAST,
            },
            Extent {
                logical: 20,
                physical: 200,
                length: 10,
                flags: 0,
            },
        ];
        assert_eq!(filefrag_extent_count(&extents), 2);
    }

    #[test]
    fn extent_count_does_not_merge_unknown_physical_locations() {
        let extents = [
            Extent {
                logical: 0,
                physical: 0,
                length: 10,
                flags: FIEMAP_EXTENT_UNKNOWN,
            },
            Extent {
                logical: 10,
                physical: 0,
                length: 10,
                flags: FIEMAP_EXTENT_UNKNOWN,
            },
        ];
        assert_eq!(filefrag_extent_count(&extents), 2);
    }

    #[test]
    fn dedupe_ranges_are_bounded_and_block_aligned() {
        let block = 4096;
        let length = MAX_DEDUPE_CHUNK + 3 * block;
        let mut offset = 0;
        let mut chunks = Vec::new();
        while offset < length {
            let chunk = aligned_chunk(length - offset, MAX_DEDUPE_CHUNK, block).unwrap();
            chunks.push(chunk);
            offset += chunk;
        }

        assert!(chunks.iter().all(|chunk| *chunk <= MAX_DEDUPE_CHUNK));
        assert!(chunks.iter().all(|chunk| chunk % block == 0));
        assert_eq!(chunks.iter().sum::<u64>(), length);
    }

    #[test]
    fn aligned_chunk_rejects_zero_or_sub_block_ranges() {
        assert_eq!(aligned_chunk(0, MAX_DEDUPE_CHUNK, 4096), None);
        assert_eq!(aligned_chunk(4095, MAX_DEDUPE_CHUNK, 4096), None);
        assert_eq!(aligned_chunk(4096, MAX_DEDUPE_CHUNK, 4096), Some(4096));
    }

    #[test]
    fn differing_ranges_alone_do_not_make_dedupe_incomplete() {
        let summary = DedupeSummary {
            differing_ranges: 1,
            ..DedupeSummary::default()
        };
        assert!(!summary.is_incomplete());
    }

    #[test]
    fn skipped_or_failed_ranges_make_dedupe_incomplete() {
        let skipped = DedupeSummary {
            skipped_bytes: 1,
            ..DedupeSummary::default()
        };
        let failed = DedupeSummary {
            failed_ranges: 1,
            ..DedupeSummary::default()
        };
        assert!(skipped.is_incomplete());
        assert!(failed.is_incomplete());
    }

    #[test]
    fn fiemap_paginates_sparse_extents() {
        let mut file = test_file().unwrap();
        let block_size = filesystem_block_size(&file).unwrap();
        let count = FIEMAP_BATCH_SIZE + 3;
        for index in 0..count {
            file.seek(SeekFrom::Start(index as u64 * block_size * 2))
                .unwrap();
            file.write_all(&vec![index as u8; block_size as usize])
                .unwrap();
        }
        file.set_len(count as u64 * block_size * 2).unwrap();
        file.sync_all().unwrap();

        let extents = match fiemap(&file) {
            Ok(extents) => extents,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP)
                ) =>
            {
                return;
            }
            Err(error) => panic!("FIEMAP failed: {error}"),
        };
        assert!(extents.len() > FIEMAP_BATCH_SIZE);
        assert_eq!(extents[0].logical, 0);
    }

    #[test]
    fn sparse_copy_preserves_contents_and_length() {
        let mut source = test_file().unwrap();
        let mut target = test_file().unwrap();
        let content = b"staged file contents";
        source.seek(SeekFrom::Start(8192)).unwrap();
        source.write_all(content).unwrap();
        source.set_len(16384).unwrap();
        source.sync_all().unwrap();

        copy_contents(&mut source, &mut target, 16384).unwrap();
        target.seek(SeekFrom::Start(0)).unwrap();
        let mut copied = Vec::new();
        target.read_to_end(&mut copied).unwrap();

        assert_eq!(copied.len(), 16384);
        assert!(copied[..8192].iter().all(|byte| *byte == 0));
        assert_eq!(&copied[8192..8192 + content.len()], content);
        assert!(copied[8192 + content.len()..].iter().all(|byte| *byte == 0));
    }
}
