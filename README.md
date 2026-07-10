# Rust File Compressor

`rzc` is a fast Rust command-line compressor. It stores data in a compact `.rzst` container and uses [zstd](https://facebook.github.io/zstd/) to beat classic `zip -9` on both speed and compression ratio for text-heavy inputs.

**Version:** 0.2.0

## Features

- **Library + CLI** — core API in `rust_file_compressor`, binary `rzc`
- **Container format v2** — RZC1 magic, version byte, compression level, original size, **SHA-256** of the original payload
- **Backward compatible** — still reads version 1 files (no checksum)
- **Integrity** — `verify` decompresses to a sink and checks size + checksum
- **`info`** — inspect magic, version, level, sizes, ratio, checksum presence
- **Recursive mode** — compress/decompress whole directory trees (`-r`)
- **Stdin/stdout** — use `-` as input or output
- **Presets** — `--preset fast|balanced|max` → levels 3 / 12 / 19
- **Progress bars** — via `indicatif` on large files when stderr is a TTY
- **Benchmark** — compare against `zip -9` on the same input
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
rzc compress large.txt -o archive.rzst
rzc compress large.txt --level 5 --threads 4
rzc compress large.txt --preset max
rzc compress data/ --recursive
```

### Decompress

```sh
rzc decompress large.txt.rzst
rzc decompress archive.rzst -o restored.txt
rzc decompress data/ --recursive
```

### Info & verify

```sh
rzc info large.txt.rzst
rzc verify large.txt.rzst
```

### Stdin / stdout

```sh
# Compress stdin to a file
cat report.txt | rzc compress - -o report.txt.rzst

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
```

`--threads 0` uses the available CPU count, capped at 8.

### Options summary

| Flag | Applies to | Description |
|------|------------|-------------|
| `-o`, `--output` | compress, decompress | Output path, or `-` for stdout |
| `-l`, `--level` | compress, bench | zstd level 1–22 (overrides preset) |
| `--preset` | compress | `fast` (3), `balanced` (12), `max` (19) |
| `-t`, `--threads` | compress, bench | zstd worker threads (`0` = auto) |
| `-r`, `--recursive` | compress, decompress | Process a directory tree |
| `--keep-artifacts` | bench | Keep temporary `.rzst` / `.zip` files |

## Container format

Magic remains **`RZC1`**. The version byte selects layout:

### Version 2 (current)

```text
Offset  Size  Field
0       4     Magic "RZC1"
4       1     Version = 2
5       4     Compression level (little-endian i32)
9       8     Original file size (little-endian u64)
17      32    SHA-256 of original payload
49      …     zstd frame
```

### Version 1 (legacy, read-only)

```text
Offset  Size  Field
0       4     Magic "RZC1"
4       1     Version = 1
5       4     Compression level (little-endian i32)
9       8     Original file size (little-endian u64)
17      …     zstd frame
```

On decompress, size is always checked. For v2, the SHA-256 of the restored payload must match the header; a mismatch fails with a clear error.

## Library usage

```rust
use rust_file_compressor::{compress_file, decompress_file, inspect_file, verify_file};
use std::path::Path;

fn example() -> anyhow::Result<()> {
    compress_file(Path::new("in.txt"), Path::new("in.txt.rzst"), 12, 0, None)?;
    let info = inspect_file(Path::new("in.txt.rzst"))?;
    println!("ratio {:.2}%", info.ratio_percent());
    verify_file(Path::new("in.txt.rzst"), None)?;
    decompress_file(Path::new("in.txt.rzst"), Path::new("out.txt"), None)?;
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
