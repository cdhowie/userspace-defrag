#[cfg(not(target_os = "linux"))]
compile_error!("userspace-defrag currently supports Linux only");

mod linux;

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Defragment a file using Linux reflink deduplication")]
struct Args {
    /// Show detailed operation diagnostics
    #[arg(short, long)]
    verbose: bool,

    /// Show FIEMAP record and physical extent maps for troubleshooting
    #[arg(long)]
    debug: bool,

    /// Skip the automatic refinement pass
    #[arg(long)]
    fast: bool,

    /// Path to a regular file on a filesystem supporting FIEMAP and FIDEDUPERANGE
    path: PathBuf,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("userspace-defrag: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let verbose = args.verbose || args.debug;
    let path = fs::canonicalize(&args.path)
        .map_err(|error| io_context(error, format!("could not resolve {}", args.path.display())))?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target has no parent directory",
        )
    })?;

    let mut original = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| io_context(error, format!("could not open {}", path.display())))?;
    if !original.metadata()?.file_type().is_file() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "target is not a regular file").into(),
        );
    }

    let original_extents = linux::fiemap(&original)
        .map_err(|error| io_context(error, "could not enumerate original extents"))?;
    let before_count = linux::filefrag_extent_count(&original_extents);
    if verbose {
        eprintln!("original extents before: {before_count}");
    }
    if args.debug {
        eprintln!(
            "original before: {} FIEMAP records, {before_count} coalesced extents",
            original_extents.len()
        );
    }

    let directory = File::open(parent).map_err(|error| {
        io_context(
            error,
            format!("could not open target directory {}", parent.display()),
        )
    })?;
    let original_size = original.metadata()?.len();
    let block_size = linux::filesystem_block_size(&original)
        .map_err(|error| io_context(error, "could not determine filesystem block size"))?;

    let mut result = run_pass(
        &mut original,
        &directory,
        original_size,
        block_size,
        1,
        verbose,
        args.debug,
    )?;
    let first_after_count = linux::filefrag_extent_count(&result.after_extents);
    let staging_count = linux::filefrag_extent_count(&result.staged_extents);

    if !args.fast
        && result.dedupe.failed_ranges == 0
        && result.dedupe.skipped_bytes == 0
        && should_refine(first_after_count, staging_count)
    {
        if verbose {
            eprintln!(
                "refining once: target has {first_after_count} extents, staging had {staging_count}"
            );
        }
        result = run_pass(
            &mut original,
            &directory,
            original_size,
            block_size,
            2,
            verbose,
            args.debug,
        )?;
    }

    let after_count = linux::filefrag_extent_count(&result.after_extents);

    if verbose {
        eprintln!("original extents after: {after_count}");
    } else {
        let net_change = if after_count >= before_count {
            format_args!("+{}", after_count - before_count)
        } else {
            format_args!("-{}", before_count - after_count)
        };
        eprintln!("extents: {before_count} before, {after_count} after (net change: {net_change})");
    }
    if verbose {
        eprintln!("deduplicated bytes: {}", result.dedupe.bytes_deduped);
        eprintln!("skipped extents: {}", result.dedupe.skipped_extents);
        eprintln!("skipped bytes: {}", result.dedupe.skipped_bytes);
        eprintln!("differing ranges: {}", result.dedupe.differing_ranges);
        eprintln!("failed ranges: {}", result.dedupe.failed_ranges);
    }
    if result.dedupe.is_incomplete() {
        return Err(io::Error::other("deduplication was incomplete").into());
    }
    Ok(())
}

fn should_refine(target_extents: usize, staging_extents: usize) -> bool {
    target_extents > staging_extents
}

struct PassResult {
    staged_extents: Vec<linux::Extent>,
    after_extents: Vec<linux::Extent>,
    dedupe: linux::DedupeSummary,
}

fn run_pass(
    original: &mut File,
    directory: &File,
    original_size: u64,
    block_size: u64,
    pass: usize,
    verbose: bool,
    debug: bool,
) -> Result<PassResult, Box<dyn Error>> {
    if debug && pass > 1 {
        let before = linux::fiemap(original).map_err(|error| {
            io_context(
                error,
                "could not enumerate original extents before refinement",
            )
        })?;
        linux::debug_extent_map("original before refinement", &before);
    }

    let mut temp = linux::open_unnamed_temp(directory)?;
    let sparse_copy = linux::copy_contents(original, &mut temp, original_size)
        .map_err(|error| io_context(error, "could not copy target into staging file"))?;
    if original.metadata()?.len() != original_size {
        return Err(io::Error::other("target size changed while it was being copied").into());
    }
    if verbose && !sparse_copy {
        eprintln!(
            "warning: SEEK_DATA/SEEK_HOLE is unsupported; the staging copy may densify holes"
        );
    }
    temp.sync_all()
        .map_err(|error| io_context(error, "could not sync staging file"))?;

    let staged_extents = linux::fiemap(&temp)
        .map_err(|error| io_context(error, "could not enumerate staging extents"))?;
    if verbose {
        eprintln!("pass {pass}: staging extents: {}", staged_extents.len());
    }
    if debug {
        linux::debug_extent_map(&format!("pass {pass} staging"), &staged_extents);
    }

    let dedupe = linux::dedupe_extents(&temp, original, &staged_extents, block_size, verbose);
    original
        .sync_all()
        .map_err(|error| io_context(error, "could not sync original after deduplication"))?;
    drop(temp);

    let after_extents = linux::fiemap(original).map_err(|error| {
        io_context(
            error,
            "could not measure original extents after deduplication",
        )
    })?;
    if verbose {
        eprintln!(
            "pass {pass}: original extents after: {}",
            linux::filefrag_extent_count(&after_extents)
        );
        eprintln!("pass {pass}: deduplicated bytes: {}", dedupe.bytes_deduped);
        eprintln!("pass {pass}: skipped extents: {}", dedupe.skipped_extents);
        eprintln!("pass {pass}: skipped bytes: {}", dedupe.skipped_bytes);
        eprintln!("pass {pass}: differing ranges: {}", dedupe.differing_ranges);
        eprintln!("pass {pass}: failed ranges: {}", dedupe.failed_ranges);
    }
    if debug {
        linux::debug_extent_map(&format!("pass {pass} original after"), &after_extents);
    }

    Ok(PassResult {
        staged_extents,
        after_extents,
        dedupe,
    })
}

fn io_context(error: io::Error, context: impl std::fmt::Display) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::should_refine;

    #[test]
    fn refinement_is_requested_when_target_is_more_fragmented() {
        assert!(should_refine(2_560, 53));
        assert!(!should_refine(53, 53));
        assert!(!should_refine(40, 53));
    }
}
