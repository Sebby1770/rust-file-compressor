# Changelog

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
