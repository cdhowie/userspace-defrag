# userspace-defrag

`userspace-defrag` is a tool that attempts to defragment a file on a filesystem
that supports reflinks and `FIDEDUPERANGE`. It accomplishes this by copying the
contents of a file into a temporary file and then using `FIDEDUPERANGE` to try
to deduplicate the temporary file's extents back into the original file. This
makes the operation safe even with concurrent writes and it also preserves the
original file's inode.

The motivation is to provide a facility similar to `btrfs filesystem defragment`
for bcachefs until it has this feature itself. However, the approach should
theoretically work on any filesystem that supports reflinks.

## Requirements and limitations

- Linux and a regular file on a filesystem supporting `FIEMAP` and
  `FIDEDUPERANGE`/reflinks.
- The filesystem must support `O_TMPFILE`. There is no named temporary-file
  fallback.
- Because `FIDEDUPERANGE` requires the destination fd to be opened with write
  access, the user running this tool must have write permissions to the file to
  be defragmented.
- Like defragmentation in btrfs, this tool's defragmentation method is not
  "snapshot-aware." Any successfully-defragmented shared extents in the file
  will become unshared.
- Dedupe needs aligned ranges, and the tool skips unsupported, unaligned, or
  sub-block extents. Filesystem request limits can also prevent a range from
  being deduplicated. Skipped data is reported and results in a nonzero exit
  status.
- Defragmentation is best-effort: Each dedupe range is compared atomically with
  concurrent writes by `FIDEDUPERANGE`. This makes the operation safe with
  respect to concurrent write activity: if a section of the original file
  changes before being deduplicated with the staging file, deduplication of that
  region will fail and therefore not be defragmented. The mismatch will be
  reported to stderr if this happens, but it will not cause a nonzero exit
  status.

## Operational behavior and diagnostics

- Before/after extent diagnostics merge adjacent `FIEMAP` records when both their
  logical and physical ranges are contiguous, matching `filefrag`'s extent
  counting. Extent counts remain a diagnostic, not a universal fragmentation
  score.
- By default, the tool performs at most two passes. Each pass copies the file
  into a fresh staging file and uses that copy's layout as a deduplication
  source. The first pass may not be able to reproduce the staging layout fully,
  leaving the target with more coalesced extents than the staging copy. In that
  case, a second pass stages the improved target and tries to deduplicate the
  remaining fragmentation. This refinement can improve the result, but does
  not guarantee a globally minimal extent count. Use `--fast` to skip this
  second attempt.
- The staging copy preserves sparse holes when `SEEK_DATA`/`SEEK_HOLE` are
  available. If they are unsupported, the tool warns that the copy may densify
  holes.

## Usage

```text
userspace-defrag [--fast] [--verbose] [--debug] <PATH>
```

Without `--verbose`, the tool writes one summary to stderr with the file's
extent counts before and after the operation and their net change. Pass
`--verbose` to also show staging, dedupe, and extent diagnostics. Errors are
reported regardless of verbosity. `--debug` includes the verbose output plus
raw FIEMAP record and coalesced extent counts and the logical/physical range
maps for the staging file and target after deduplication. This can produce
substantial stderr output for highly fragmented files.
