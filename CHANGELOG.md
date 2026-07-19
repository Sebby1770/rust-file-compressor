# Changelog

## 0.4.0 — 2026-07-19

### Added

- **Selective unpack** — `rzc unpack archive.rzst --only path/in/archive -o out/`
- **`--strip-components N`** on unpack (like `tar --strip-components`)
- **`rzc cat archive.rzst path/inside`** — decompress one pack member (or a single-file archive) to stdout
- **`rzc diff a.rzst b.rzst`** — compare member lists and checksums (pack) or single-file hashes; exit 1 on differences
- **`--force`** — require explicit overwrite for compress/pack outputs and unpack destinations that already exist
- **`--newer-than DAYS`** on pack — only include files modified within the last N days
- **Solid pack progress** — multi-file progress bar by file count (`N/M files`)
- **Compression ratio table** after recursive compress (per-file + TOTAL)
- **Shell completions** — `rzc completions bash|zsh|fish|powershell|elvish` via `clap_complete`
- Library APIs: `unpack_archive_opts`, `pack_directory_opts`, `cat_member`, `extract_pack_member`, `diff_archives`, `compress_file_opts`, `strip_path_components`, `UnpackOpts`, `PackOpts`, `DiffResult`
- Tests for only, strip-components, diff, cat, force, newer-than

### Changed

- Version bumped to **0.4.0**
- Compress/pack refuse to overwrite an existing output path without `--force`
- Unpack refuses to overwrite an existing member file without `--force` (or `--skip-existing` to leave it)

### Notes

- v1/v2 single-file and v3 pack formats remain fully compatible
- Append/update-in-place pack was deferred (format stores file count up front)

## 0.3.0 — 2026-07-19

### Added

- **Format v3 multi-file pack archives**: `rzc pack dir/ -o bundle.rzst` and `rzc unpack bundle.rzst -o outdir/`
  - Layout: RZC1 magic, version 3, level, file count, then per-member path / original_len / sha256 / compressed payload
- **`rzc list`** — show member paths and sizes for pack archives; single-file summary for v1/v2
- **`--dry-run`** on compress — estimate compressed size without writing
- **`--exclude GLOB`** on recursive compress and pack (repeatable)
- **`--skip-existing`** on decompress and unpack
- **`--json`** on info, list, pack, unpack, bench, and doctor
- **`rzc doctor`** — in-memory / temp roundtrip self-test for zstd, v2, and v3
- Library APIs: `pack_directory`, `unpack_archive`, `list_archive`, `inspect_pack`, `compress_file_dry_run`, `doctor`, exclude helpers
- Tests for pack/unpack, exclude, dry-run, list, doctor, skip-existing, path traversal

### Changed

- Version bumped to **0.3.0**
- Single-file `compress` remains **format v2** (backward compatible)
- `info` / `verify` understand pack archives
- Dependencies: `serde`, `serde_json`, `globset`

### Notes

- v1 single-file archives remain readable
- Unpack rejects archive paths containing `..` (path traversal)
