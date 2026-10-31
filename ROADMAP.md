# fcopy roadmap

Twenty updates, grouped by area and roughly in priority order within each group.
Items 1–3 are done. Status was checked against the code on 2026-10-31.

## Done

1. **Move mode.** Copy, sync and read back each file, then delete its source.
   Files that are not copied keep their source, emptied source folders are
   removed, and moving off another PC is refused. In the engine, CLI
   (`--move`), desktop and web console.
2. **Faster small files on Windows.** Files under 1 MiB now copy on 4 workers:
   20,000 × 128 B went from 22.3 s to 17.0 s (medians of 5). A `CopyFileExW` fast
   path was measured and dropped: it was slower once it matched the normal
   path's results. Still far from Linux (1.8 s); see items 5 and 6.
3. **Save a checksum list from a finished job.** Desktop: the `#` button on a
   finished job writes a list into its destination folder. Web console: a Verify
   choice when adding a transfer, and a Checksums button that downloads the
   list. Both check with `fcopy check --algo <ext> <list>`.

## Copy engine

4. **Instant moves on the same drive.** Rename instead of copying when source
   and destination share a volume, falling back to copy-then-delete across drives.
5. **Copy-on-write and kernel fast paths.** Reflink on Btrfs/XFS, `clonefile`
   on APFS, block cloning on ReFS, `copy_file_range` on Linux. Listed in the
   performance to-dos in CLAUDE.md.
6. **Batched durability for read-back verify.** Today each verified file is
   synced on its own, which dominates small-file jobs; batch the syncs
   (`syncfs`, or one sync per folder) without weakening the guarantee.
7. **Mirror / sync mode.** Copy only new and changed files, with an option to
   delete files that no longer exist in the source (like `robocopy /MIR`), and a
   dry-run that lists what would change.
8. **Include and exclude filters.** Glob patterns, size and date limits, and
   "skip hidden/system files", in the CLI, desktop and web console.
9. **Verify-only jobs.** Compare a folder against its copy, or against a
   checksum list, without copying anything (TeraCopy's "Test").
10. **Job reports.** Export a finished job's copied, skipped and failed files,
    with sizes and checksums, as CSV or HTML.

## Desktop and web console

11. **Scheduling.** Start a job at a set time, and apply speed limits by time of
    day (for example, full speed overnight).
12. **After-job actions.** Notify, sleep or shut down when the queue finishes.
13. **Windows 11 modern right-click menu.** Needs `IExplorerCommand` and a signed
    sparse package; noted as "not planned yet" in CLAUDE.md.
14. **Take over Explorer drag-and-drop copies (investigate).** Optional, off by
    default, and only if Windows allows it to be done cleanly.
15. **Accessibility.** Full keyboard navigation and screen-reader labels in the
    desktop app and the web console.

## Command line

16. **Machine-readable output.** A `--json` mode that prints progress and the
    final report as JSON lines for scripts and CI.

## BitTorrent

17. **Choose which files to download** from a multi-file torrent (librqbit
    supports per-file selection).
18. **Watch folder for .torrent files,** added automatically, with an optional
    feed reader later.

## Distribution

19. **Signed releases.** The v0.1.1 Windows installer is unsigned; sign Windows
    builds, and confirm macOS builds are signed and notarized.
20. **Update checks.** Tell the user when a newer GitHub release is available,
    and never download or install without asking.
