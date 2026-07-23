# Changelog

## [0.6.0] — 2026-07-22

### Added
- **zstd dictionaries** — `rzc dict train` trains a dictionary from a corpus of
  at least 8 similar files (`--max-size` caps it); `--dictionary` applies it on
  `compress` and `decompress` for far better ratios on many small files. The v2
  container is unchanged — a dictionary-compressed payload is a standard zstd
  frame — so the dictionary is not stored and must be retained to decode.
- **Parallel recursive compress** — `compress --recursive` compresses independent
  files across cores with rayon; output is sorted by path for stable results.
- Library APIs: `compress_bytes_dict`, `compress_file_opts_dict`,
  `decompress_reader_dict`, `decompress_file_opts_dict`, `decompress_to_path_dict`,
  and the `dict` module (`train_dictionary`, `DEFAULT_MAX_DICT_SIZE`).

### Fixed
- **Denial of service on malformed pack archives** — every v3 member reader passed
  an attacker-controlled length straight to `vec![0; declared]`. A 69-byte archive
  declaring a 281 TB payload aborted the process (`memory allocation … failed`)
  before any validation. Member blobs are now read through a bounded reader, so an
  absurd length is an ordinary error; `list` also verifies it skipped a member's
  full declared payload rather than reporting a truncated archive as valid.
- CI is green again: 10 `rustfmt` violations and a `clippy::too_many_arguments`
  error in `run_unpack` (present on `main`) are resolved.

### Changed
- Version **0.6.0**
- Single-file v2 and pack v3 formats remain fully compatible

## [0.5.0] — 2026-07-21

### Added
- **`rzc tree`** — directory tree of pack members (v3)
- **`rzc grep`** — search decompressed text members (regex, size cap)
- **`rzc seal` / `rzc check`** — SHA-256 integrity sidecar (`.rzst.sha256`)
- **`rzc repack`** — rewrite packs with `--exclude` globs (no recompress)
- **Global `-q` / `--quiet`** — quieter CLI
- **`format_duration`** helper for human-readable timings
- Expanded library tests for tree, grep, seal/check, repack

### Changed
- Version **0.5.0**
- Single-file v2 and pack v3 remain fully compatible


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
