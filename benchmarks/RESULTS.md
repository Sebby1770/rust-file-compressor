# Benchmark Results

Benchmark command:

```sh
python3 scripts/make_sample_text.py --output benchmarks/sample-large.txt --size-mib 40
cargo run --release -- bench benchmarks/sample-large.txt
```

Local result:

```text
tool                 size      ratio       time     throughput
rzc-l12          5.88 MiB     14.70%     0.293s    136.35 MiB/s
zip-9            6.43 MiB     16.07%     0.905s     44.20 MiB/s

Result: rzc output was 8.53% smaller than zip -9 and 3.08x faster.
```

Notes:

- Input: 40 MiB deterministic generated UTF-8 text corpus.
- `rzc` settings: zstd level 12, automatic worker threads capped at 8.
- `zip` settings: `zip -q -j -9`.
- Results vary by machine and input data.
