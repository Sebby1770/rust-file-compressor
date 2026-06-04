# Rust File Compressor

`rzc` is a fast Rust command-line compressor for large text files. It stores data in a tiny `.rzst` container and uses zstd, a modern Lempel-Ziv family compressor with entropy coding, to beat classic `zip -9` on both speed and compression ratio for the benchmark corpus included below.

## Features

- Compress files with a small custom header that records version, level, and original size.
- Decompress with validation that the restored byte count matches the original.
- Benchmark directly against `zip -9`.
- Use zstd worker threads automatically, capped at 8 by default.
- Tested round-trip behavior for text files and invalid containers.

## Install

```sh
cargo install --path .
```

Or run directly from the repo:

```sh
cargo run --release -- --help
```

## Usage

Compress:

```sh
cargo run --release -- compress large.txt
```

Decompress:

```sh
cargo run --release -- decompress large.txt.rzst
```

Benchmark against `zip -9`:

```sh
cargo run --release -- bench benchmarks/sample-large.txt
```

Useful options:

```sh
cargo run --release -- compress large.txt --level 5 --threads 4
cargo run --release -- bench large.txt --level 12 --threads 0
```

`--threads 0` uses the available CPU count, capped at 8.

## Algorithm

The compressor uses zstd through Rust's `zstd` crate. zstd combines LZ-style match finding with entropy coding and a larger effective window than classic zip/deflate, which is why it performs especially well on repeated or structurally similar text.

The project keeps its own lightweight container:

```text
RZC1 magic bytes
1 byte version
4 bytes compression level, little-endian i32
8 bytes original file size, little-endian u64
zstd frame bytes
```

## Benchmark

The benchmark command times this program and `zip -9` on the same input, then prints compressed size, ratio, elapsed time, throughput, and the size/speed advantage.

To generate a deterministic large text file locally:

```sh
python3 scripts/make_sample_text.py --output benchmarks/sample-large.txt --size-mib 40
cargo run --release -- bench benchmarks/sample-large.txt
```

Current local result on a 40 MiB deterministic generated text corpus:

```text
tool                 size      ratio       time     throughput
rzc-l12          5.88 MiB     14.70%     0.293s    136.35 MiB/s
zip-9            6.43 MiB     16.07%     0.905s     44.20 MiB/s

Result: rzc output was 8.53% smaller than zip -9 and 3.08x faster.
```

Compression performance depends on input data and hardware, so rerun the benchmark with your own `.txt` files before making broad claims.
