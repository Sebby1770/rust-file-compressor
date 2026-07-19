# Rust File Compressor

`rzc` is a fast Rust command-line compressor. It stores data in a compact `.rzst` container and uses [zstd](https://facebook.github.io/zstd/) to beat classic `zip -9` on both speed and compression ratio for text-heavy inputs.

**Version:** 0.4.0

## Features

- **Library + CLI** — core API in `rust_file_compressor`, binary `rzc`
- **Single-file format v2** — RZC1 magic, version, level, original size, **SHA-256**
- **Multi-file pack archives (v3)** — `pack` / `unpack` directory trees into one `.rzst`
- **Selective unpack** — `--only path` and `--strip-components N`
- **`cat` / `diff`** — stream one member to stdout; compare two archives by checksums
- **Backward compatible** — still reads version 1 files (no checksum)
- **`list` / `info`** — inspect members, sizes, ratios (optional `--json`)
- **Integrity** — `verify` decompresses to a sink and checks size + checksum (single + pack)
- **Recursive mode** — compress/decompress whole directory trees (`-r`) with `--exclude` globs and a ratio summary table
- **Dry-run** — `--dry-run` estimates compressed size without writing
- **`--force` / `--skip-existing`** — control overwrite behaviour
- **Age filter** — pack with `--newer-than DAYS`
- **Stdin/stdout** — use `-` as input or output
- **Presets** — `--preset fast|balanced|max` → levels 3 / 12 / 19
- **Progress bars** — via `indicatif` on large files and multi-file packs when stderr is a TTY
- **Shell completions** — `rzc completions bash|zsh|fish|…`
- **Benchmark** — compare against `zip -9` (`--json` supported)
- **`doctor`** — self-test zstd + container roundtrips
- **Multi-threaded zstd** — worker threads auto-selected (capped at 8 by default)

## Install

```sh
cargo install --path .
```

Or run from the repo:

```sh
cargo run --release -- --help
```

## Usage

### Compress

```sh
rzc compress large.txt
rzc compress large.txt -o archive.rzst --force
rzc compress large.txt --level 5 --threads 4
rzc compress large.txt --preset max
rzc compress large.txt --dry-run
rzc compress data/ --recursive
rzc compress data/ -r --exclude 'target' --exclude '*.git*'
```

Recursive compress prints a **ratio table** (per file + TOTAL).

### Decompress

```sh
rzc decompress large.txt.rzst
rzc decompress archive.rzst -o restored.txt
rzc decompress data/ --recursive
rzc decompress archive.rzst --skip-existing
```

### Pack / unpack (multi-file archive)

```sh
rzc pack myproject/ -o bundle.rzst
rzc pack myproject/ --exclude target --exclude '.git' --preset fast
rzc pack myproject/ --newer-than 7 -o recent.rzst
rzc pack myproject/ -o bundle.rzst --force

rzc unpack bundle.rzst -o restored/
rzc unpack bundle.rzst -o restored/ --only src/main.rs
rzc unpack bundle.rzst -o restored/ --strip-components 1
rzc unpack bundle.rzst -o restored/ --skip-existing
rzc unpack bundle.rzst -o restored/ --force
```

### Cat / diff

```sh
# Stream one pack member to stdout
rzc cat bundle.rzst src/main.rs > main.rs

# Single-file archive to stdout
rzc cat large.txt.rzst

# Compare two archives (exit 1 if they differ)
rzc diff a.rzst b.rzst
rzc diff a.rzst b.rzst --json
```

### List, info, verify, doctor, completions

```sh
rzc list bundle.rzst
rzc list archive.rzst --json
rzc info large.txt.rzst
rzc info bundle.rzst --json
rzc verify large.txt.rzst
rzc verify bundle.rzst
rzc doctor
rzc doctor --json

# Shell completions
rzc completions bash > /etc/bash_completion.d/rzc
rzc completions zsh > ~/.zfunc/_rzc
rzc completions fish > ~/.config/fish/completions/rzc.fish
```

### Stdin / stdout

```sh
# Compress stdin to a file
cat report.txt | rzc compress - -o report.txt.rzst --force

# Compress file to stdout
rzc compress report.txt -o - > report.txt.rzst

# Round-trip through pipes
cat report.txt | rzc compress - | rzc decompress - > report.out.txt
```

Stdin is fully buffered into memory so the v2 header can record size and SHA-256 before the zstd frame.

### Benchmark against `zip -9`

```sh
rzc bench benchmarks/sample-large.txt
rzc bench large.txt --level 12 --threads 0 --keep-artifacts
rzc bench large.txt --json
```

`--threads 0` uses the available CPU count, capped at 8.

### Options summary

| Flag | Applies to | Description |
|------|------------|-------------|
| `-o`, `--output` | compress, decompress, pack, unpack | Output path / directory |
| `-l`, `--level` | compress, pack, bench | zstd level 1–22 (overrides preset) |
| `--preset` | compress, pack | `fast` (3), `balanced` (12), `max` (19) |
| `-t`, `--threads` | compress, pack, bench | zstd worker threads (`0` = auto) |
| `-r`, `--recursive` | compress, decompress | Process a directory tree |
| `--exclude GLOB` | compress `-r`, pack | Skip matching paths (repeatable) |
| `--newer-than DAYS` | pack | Only files modified in the last N days |
| `--dry-run` | compress | Estimate size without writing |
| `--force` | compress, pack, unpack | Overwrite existing outputs |
| `--skip-existing` | decompress, unpack | Do not overwrite existing files |
| `--only PATH` | unpack | Extract a single member |
| `--strip-components N` | unpack | Strip N leading path components |
| `--json` | info, list, pack, unpack, diff, bench, doctor | Machine-readable output |
| `--keep-artifacts` | bench | Keep temporary `.rzst` / `.zip` files |

## Container format

Magic remains **`RZC1`**. The version byte selects layout:

### Version 2 (single-file, current default for `compress`)

```text
Offset  Size  Field
0       4     Magic "RZC1"
4       1     Version = 2
5       4     Compression level (little-endian i32)
9       8     Original file size (little-endian u64)
17      32    SHA-256 of original payload
49      …     zstd frame
```

### Version 3 (multi-file pack archive)

```text
Offset  Size  Field
0       4     Magic "RZC1"
4       1     Version = 3
5       4     Compression level (little-endian i32)
9       4     File count (little-endian u32)
13      …     Members (repeated file_count times):
              path_len (u32 LE)
              path (UTF-8, relative, `/` separators)
              original_len (u64 LE)
              sha256 (32 bytes)
              compressed_len (u64 LE)
              compressed_bytes (raw zstd frame)
```

Paths are relative, use `/`, and must not contain `..`. Unpack rejects path traversal.

### Version 1 (legacy single-file, read-only)

```text
Offset  Size  Field
0       4     Magic "RZC1"
4       1     Version = 1
5       4     Compression level (little-endian i32)
9       8     Original file size (little-endian u64)
17      …     zstd frame
```

On decompress, size is always checked. For v2/v3, the SHA-256 of the restored payload must match; a mismatch fails with a clear error.

## Library usage

```rust
use rust_file_compressor::{
    cat_member, compress_file, decompress_file, diff_archives, inspect_file,
    list_archive, pack_directory, unpack_archive, unpack_archive_opts, verify_file,
    doctor, UnpackOpts,
};
use std::path::Path;

fn example() -> anyhow::Result<()> {
    compress_file(Path::new("in.txt"), Path::new("in.txt.rzst"), 12, 0, None)?;
    let info = inspect_file(Path::new("in.txt.rzst"))?;
    println!("ratio {:.2}%", info.ratio_percent());
    verify_file(Path::new("in.txt.rzst"), None)?;

    pack_directory(Path::new("project/"), Path::new("project.rzst"), 12, 0, &[])?;
    unpack_archive_opts(
        Path::new("project.rzst"),
        Path::new("out/"),
        &UnpackOpts {
            only: Some("src/main.rs".into()),
            force: true,
            ..Default::default()
        },
    )?;
    let _ = list_archive(Path::new("project.rzst"))?;
    let _ = diff_archives(Path::new("a.rzst"), Path::new("b.rzst"))?;
    let mut buf = Vec::new();
    cat_member(Path::new("project.rzst"), Some("README"), &mut buf)?;
    assert!(doctor().ok);
    Ok(())
}
```

## Algorithm

Compression uses zstd (LZ-style matching + entropy coding) via the `zstd` crate. Compared with classic zip/deflate, zstd typically offers a better ratio/speed trade-off on repeated or structured text.

## Benchmark

Generate a deterministic large text file and compare:

```sh
python3 scripts/make_sample_text.py --output benchmarks/sample-large.txt --size-mib 40
rzc bench benchmarks/sample-large.txt
```

Example local result on a 40 MiB deterministic corpus:

```text
tool                 size      ratio       time     throughput
rzc-l12          5.88 MiB     14.70%     0.293s    136.35 MiB/s
zip-9            6.43 MiB     16.07%     0.905s     44.20 MiB/s

Result: rzc output was 8.53% smaller than zip -9 and 3.08x faster.
```

Performance depends on input and hardware — rerun with your own files before drawing conclusions.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI (GitHub Actions) runs test, clippy, and fmt on push/PR.

## License

MIT
