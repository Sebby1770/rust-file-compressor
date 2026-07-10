use std::{
    fs::{self, File},
    io::{self, BufReader, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use rust_file_compressor::{
    append_suffix, bytes_per_second, compress_bytes, compress_dir_recursive, compress_file,
    compress_to_path, decompress_dir_recursive, decompress_file, decompress_reader,
    decompress_to_path, default_decompressed_path, display_bytes, inspect_file, is_stdio_path,
    ratio_percent, resolve_threads, verify_file, ArchiveInfo, IoStats, Preset, DEFAULT_LEVEL,
};

#[derive(Parser)]
#[command(
    name = "rzc",
    author,
    version,
    about = "Fast Rust file compression with integrity checks and zip benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PresetArg {
    Fast,
    Balanced,
    Max,
}

impl From<PresetArg> for Preset {
    fn from(value: PresetArg) -> Self {
        match value {
            PresetArg::Fast => Preset::Fast,
            PresetArg::Balanced => Preset::Balanced,
            PresetArg::Max => Preset::Max,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Compress a file (or directory with --recursive) to the .rzst container format.
    Compress {
        /// File, directory (with -r), or `-` for stdin.
        input: PathBuf,

        /// Output path. Defaults to <input>.rzst. Use `-` for stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// zstd compression level (overrides --preset).
        #[arg(short, long, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: Option<i32>,

        /// Compression preset: fast=3, balanced=12, max=19.
        #[arg(long, value_enum)]
        preset: Option<PresetArg>,

        /// Worker threads. 0 uses the available CPU count, capped at 8.
        #[arg(short, long, default_value_t = 0)]
        threads: u32,

        /// Recursively compress every file under a directory into sibling .rzst files.
        #[arg(short = 'r', long)]
        recursive: bool,
    },

    /// Decompress a .rzst file (or directory with --recursive).
    Decompress {
        /// File, directory (with -r), or `-` for stdin.
        input: PathBuf,

        /// Output path. Defaults to stripping .rzst, or <input>.out. Use `-` for stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Recursively decompress every .rzst under a directory.
        #[arg(short = 'r', long)]
        recursive: bool,
    },

    /// Show container metadata without fully decompressing.
    Info {
        /// .rzst file to inspect.
        input: PathBuf,
    },

    /// Verify size and checksum by decompressing to a sink.
    Verify {
        /// .rzst file to verify.
        input: PathBuf,
    },

    /// Compare this compressor against zip -9 on one input file.
    Bench {
        /// Text file to benchmark.
        input: PathBuf,

        /// zstd compression level for this compressor.
        #[arg(short, long, default_value_t = DEFAULT_LEVEL, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: i32,

        /// Worker threads. 0 uses the available CPU count, capped at 8.
        #[arg(short, long, default_value_t = 0)]
        threads: u32,

        /// Keep generated .rzst and .zip files in a temporary benchmark folder.
        #[arg(long)]
        keep_artifacts: bool,
    },
}

#[derive(Debug)]
struct TimedStats {
    output_bytes: u64,
    elapsed: Duration,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Compress {
            input,
            output,
            level,
            preset,
            threads,
            recursive,
        } => {
            let level = resolve_level(level, preset);
            let threads = resolve_threads(threads);
            run_compress(&input, output.as_deref(), level, threads, recursive)?;
        }
        Commands::Decompress {
            input,
            output,
            recursive,
        } => {
            run_decompress(&input, output.as_deref(), recursive)?;
        }
        Commands::Info { input } => {
            run_info(&input)?;
        }
        Commands::Verify { input } => {
            run_verify(&input)?;
        }
        Commands::Bench {
            input,
            level,
            threads,
            keep_artifacts,
        } => run_benchmark(&input, level, resolve_threads(threads), keep_artifacts)?,
    }

    Ok(())
}

fn resolve_level(level: Option<i32>, preset: Option<PresetArg>) -> i32 {
    if let Some(l) = level {
        l
    } else if let Some(p) = preset {
        Preset::from(p).level()
    } else {
        DEFAULT_LEVEL
    }
}

fn run_compress(
    input: &Path,
    output: Option<&Path>,
    level: i32,
    threads: u32,
    recursive: bool,
) -> Result<()> {
    if recursive {
        if is_stdio_path(input) {
            bail!("--recursive cannot be used with stdin (-)");
        }
        if !input.is_dir() {
            bail!(
                "--recursive requires a directory input; got {}",
                input.display()
            );
        }
        if output.is_some() {
            bail!("--output is not supported with --recursive (writes sibling .rzst files)");
        }
        let results = compress_dir_recursive(input, level, threads, None)?;
        if results.is_empty() {
            println!("No files compressed under {}", input.display());
        } else {
            println!(
                "Compressed {} file(s) under {}",
                results.len(),
                input.display()
            );
            for s in &results {
                println!(
                    "  {} -> {} ({:.2}%)",
                    display_bytes(s.input_bytes),
                    display_bytes(s.output_bytes),
                    ratio_percent(s.output_bytes, s.input_bytes)
                );
                println!("    {}", s.output_path.display());
            }
        }
        return Ok(());
    }

    let out_is_stdio = output.map(is_stdio_path).unwrap_or(false);
    let in_is_stdio = is_stdio_path(input);

    if in_is_stdio && output.is_none() {
        // Default stdout when reading stdin without -o.
        compress_stdio_to_stdio(level, threads)?;
        return Ok(());
    }

    if in_is_stdio {
        let output = output.expect("checked above");
        if is_stdio_path(output) {
            compress_stdio_to_stdio(level, threads)?;
        } else {
            let stats = compress_to_path(io::stdin().lock(), output, level, threads, None)?;
            print_compress_stats(&stats, Duration::ZERO, false);
        }
        return Ok(());
    }

    if out_is_stdio {
        let data = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
        let start = Instant::now();
        let mut stdout = io::stdout().lock();
        compress_bytes(&data, &mut stdout, level, threads, None)?;
        stdout.flush()?;
        // Avoid polluting stdout; print summary to stderr.
        eprintln!(
            "Compressed {} -> stdout in {:.3}s",
            display_bytes(data.len() as u64),
            start.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    let output_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| append_suffix(input, ".rzst"));

    let total = fs::metadata(input)
        .with_context(|| format!("reading metadata for {}", input.display()))?
        .len();
    let pb = make_progress_bar(total, "compressing");
    let progress = pb.as_ref().map(|bar| {
        let bar = bar.clone();
        move |done: u64, total_hint: u64| {
            if total_hint > 0 {
                bar.set_length(total_hint);
            }
            bar.set_position(done);
        }
    });
    let progress_ref = progress
        .as_ref()
        .map(|f| f as &rust_file_compressor::ProgressFn<'_>);

    let start = Instant::now();
    let stats = compress_file(input, &output_path, level, threads, progress_ref)?;
    let elapsed = start.elapsed();
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }
    print_compress_stats(&stats, elapsed, true);
    Ok(())
}

fn compress_stdio_to_stdio(level: i32, threads: u32) -> Result<()> {
    let mut data = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut data)
        .context("reading stdin")?;
    let mut stdout = io::stdout().lock();
    compress_bytes(&data, &mut stdout, level, threads, None)?;
    stdout.flush()?;
    eprintln!(
        "Compressed {} from stdin -> stdout",
        display_bytes(data.len() as u64)
    );
    Ok(())
}

fn run_decompress(input: &Path, output: Option<&Path>, recursive: bool) -> Result<()> {
    if recursive {
        if is_stdio_path(input) {
            bail!("--recursive cannot be used with stdin (-)");
        }
        if !input.is_dir() {
            bail!(
                "--recursive requires a directory input; got {}",
                input.display()
            );
        }
        if output.is_some() {
            bail!("--output is not supported with --recursive");
        }
        let results = decompress_dir_recursive(input, None)?;
        if results.is_empty() {
            println!("No .rzst files found under {}", input.display());
        } else {
            println!(
                "Decompressed {} file(s) under {}",
                results.len(),
                input.display()
            );
            for s in &results {
                println!(
                    "  {} -> {}  {}",
                    display_bytes(s.input_bytes),
                    display_bytes(s.output_bytes),
                    s.output_path.display()
                );
            }
        }
        return Ok(());
    }

    let in_is_stdio = is_stdio_path(input);
    let out_is_stdio = output.map(is_stdio_path).unwrap_or(false);

    if in_is_stdio {
        let mut reader = BufReader::new(io::stdin().lock());
        if let Some(path) = output.filter(|p| !is_stdio_path(p)) {
            let start = Instant::now();
            let stats = decompress_to_path(reader, path, None)?;
            print_decompress_stats(&stats, start.elapsed(), true);
        } else {
            let start = Instant::now();
            let written = decompress_reader(&mut reader, io::stdout().lock(), None)?;
            eprintln!(
                "Decompressed stdin -> stdout ({} in {:.3}s)",
                display_bytes(written),
                start.elapsed().as_secs_f64()
            );
        }
        return Ok(());
    }

    if out_is_stdio {
        let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
        let start = Instant::now();
        let written = decompress_reader(BufReader::new(file), io::stdout().lock(), None)?;
        eprintln!(
            "Decompressed {} -> stdout ({} in {:.3}s)",
            input.display(),
            display_bytes(written),
            start.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    let output_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_decompressed_path(input));

    let info = inspect_file(input).ok();
    let total = info.as_ref().map(|i| i.header.original_len).unwrap_or(0);
    let pb = make_progress_bar(total, "decompressing");
    let progress = pb.as_ref().map(|bar| {
        let bar = bar.clone();
        move |done: u64, total_hint: u64| {
            if total_hint > 0 {
                bar.set_length(total_hint);
            }
            bar.set_position(done);
        }
    });
    let progress_ref = progress
        .as_ref()
        .map(|f| f as &rust_file_compressor::ProgressFn<'_>);

    let start = Instant::now();
    let stats = decompress_file(input, &output_path, progress_ref)?;
    let elapsed = start.elapsed();
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }
    print_decompress_stats(&stats, elapsed, true);
    Ok(())
}

fn run_info(input: &Path) -> Result<()> {
    let info = inspect_file(input)?;
    print_archive_info(&info);
    Ok(())
}

fn print_archive_info(info: &ArchiveInfo) {
    let header = &info.header;
    println!("File:            {}", info.path.display());
    println!("Magic:           RZC1");
    println!("Version:         {}", header.version);
    println!("Level:           {}", header.level);
    println!(
        "Original size:   {} ({})",
        header.original_len,
        display_bytes(header.original_len)
    );
    println!(
        "Compressed size: {} ({})",
        info.compressed_size,
        display_bytes(info.compressed_size)
    );
    println!("Ratio:           {:.2}%", info.ratio_percent());
    match &header.checksum {
        Some(hash) => {
            println!("Checksum:        present (SHA-256)");
            println!("SHA-256:         {}", hex::encode(hash));
        }
        None => {
            println!("Checksum:        absent (v1 container)");
        }
    }
}

fn run_verify(input: &Path) -> Result<()> {
    let info = inspect_file(input)?;
    let total = info.header.original_len;
    let pb = make_progress_bar(total, "verifying");
    let progress = pb.as_ref().map(|bar| {
        let bar = bar.clone();
        move |done: u64, total_hint: u64| {
            if total_hint > 0 {
                bar.set_length(total_hint);
            }
            bar.set_position(done);
        }
    });
    let progress_ref = progress
        .as_ref()
        .map(|f| f as &rust_file_compressor::ProgressFn<'_>);

    let start = Instant::now();
    let written = verify_file(input, progress_ref)?;
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }

    println!("OK: {}", input.display());
    println!(
        "Verified {} in {:.3}s",
        display_bytes(written),
        start.elapsed().as_secs_f64()
    );
    if info.header.has_checksum() {
        println!("Checksum: valid (SHA-256)");
    } else {
        println!("Checksum: not present (v1); size check passed");
    }
    Ok(())
}

fn print_compress_stats(stats: &IoStats, elapsed: Duration, show_time: bool) {
    println!(
        "Compressed {} -> {}",
        display_bytes(stats.input_bytes),
        display_bytes(stats.output_bytes)
    );
    println!("Output: {}", stats.output_path.display());
    if show_time && !elapsed.is_zero() {
        println!(
            "Ratio: {:.2}% | Time: {:.3}s | Throughput: {}/s",
            ratio_percent(stats.output_bytes, stats.input_bytes),
            elapsed.as_secs_f64(),
            display_bytes(bytes_per_second(stats.input_bytes, elapsed))
        );
    } else {
        println!(
            "Ratio: {:.2}%",
            ratio_percent(stats.output_bytes, stats.input_bytes)
        );
    }
}

fn print_decompress_stats(stats: &IoStats, elapsed: Duration, show_time: bool) {
    println!(
        "Decompressed {} -> {}",
        display_bytes(stats.input_bytes),
        display_bytes(stats.output_bytes)
    );
    println!("Output: {}", stats.output_path.display());
    if show_time && !elapsed.is_zero() {
        println!(
            "Time: {:.3}s | Throughput: {}/s",
            elapsed.as_secs_f64(),
            display_bytes(bytes_per_second(stats.output_bytes, elapsed))
        );
    }
}

fn make_progress_bar(total: u64, msg: &str) -> Option<ProgressBar> {
    if !io::stderr().is_terminal() {
        return None;
    }
    // Only show for reasonably large work.
    if total > 0 && total < 1024 * 1024 {
        return None;
    }
    let bar = if total > 0 {
        ProgressBar::new(total)
    } else {
        ProgressBar::new_spinner()
    };
    bar.set_style(
        ProgressStyle::with_template(
            "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );
    bar.set_message(msg.to_string());
    Some(bar)
}

fn run_benchmark(input: &Path, level: i32, threads: u32, keep_artifacts: bool) -> Result<()> {
    ensure_zip_available()?;

    let input_bytes = fs::metadata(input)
        .with_context(|| format!("reading metadata for {}", input.display()))?
        .len();
    let bench_dir = benchmark_dir();
    fs::create_dir_all(&bench_dir)
        .with_context(|| format!("creating benchmark directory {}", bench_dir.display()))?;

    let rzc_output = bench_dir.join("input.rzst");
    let zip_output = bench_dir.join("input.zip");

    let rzc = timed_compress(input, &rzc_output, level, threads)?;
    let zip = timed_zip(input, &zip_output)?;

    println!(
        "Input: {} ({})",
        input.display(),
        display_bytes(input_bytes)
    );
    println!("Benchmark artifacts: {}", bench_dir.display());
    println!();
    println!(
        "{:<12} {:>12} {:>10} {:>10} {:>14}",
        "tool", "size", "ratio", "time", "throughput"
    );
    println!(
        "{:<12} {:>12} {:>9.2}% {:>9.3}s {:>13}/s",
        format!("rzc-l{level}"),
        display_bytes(rzc.output_bytes),
        ratio_percent(rzc.output_bytes, input_bytes),
        rzc.elapsed.as_secs_f64(),
        display_bytes(bytes_per_second(input_bytes, rzc.elapsed)),
    );
    println!(
        "{:<12} {:>12} {:>9.2}% {:>9.3}s {:>13}/s",
        "zip-9",
        display_bytes(zip.output_bytes),
        ratio_percent(zip.output_bytes, input_bytes),
        zip.elapsed.as_secs_f64(),
        display_bytes(bytes_per_second(input_bytes, zip.elapsed)),
    );
    println!();

    let size_delta = describe_size_delta(zip.output_bytes, rzc.output_bytes);
    let speed_delta = describe_speed_delta(zip.elapsed, rzc.elapsed);
    println!("Result: rzc output was {size_delta} than zip -9 and {speed_delta}.");

    if !keep_artifacts {
        fs::remove_dir_all(&bench_dir)
            .with_context(|| format!("removing benchmark directory {}", bench_dir.display()))?;
    }

    Ok(())
}

fn timed_compress(input: &Path, output: &Path, level: i32, threads: u32) -> Result<TimedStats> {
    let start = Instant::now();
    let stats = compress_file(input, output, level, threads, None)?;
    let elapsed = start.elapsed();
    Ok(TimedStats {
        output_bytes: stats.output_bytes,
        elapsed,
    })
}

fn timed_zip(input: &Path, output: &Path) -> Result<TimedStats> {
    let start = Instant::now();
    let status = Command::new("zip")
        .arg("-q")
        .arg("-j")
        .arg("-9")
        .arg(output)
        .arg(input)
        .stdin(Stdio::null())
        .status()
        .context("running zip -9")?;
    let elapsed = start.elapsed();

    if !status.success() {
        bail!("zip -9 exited with status {status}");
    }

    Ok(TimedStats {
        output_bytes: fs::metadata(output)
            .with_context(|| format!("reading metadata for {}", output.display()))?
            .len(),
        elapsed,
    })
}

fn ensure_zip_available() -> Result<()> {
    let status = Command::new("zip")
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("checking for zip command")?;
    if status.success() {
        Ok(())
    } else {
        bail!("zip command exists but did not run successfully")
    }
}

fn benchmark_dir() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    std::env::temp_dir().join(format!("rzc-bench-{}-{millis}", std::process::id()))
}

fn describe_size_delta(baseline: u64, measured: u64) -> String {
    if baseline == 0 {
        return "0.00% different".to_string();
    }

    let delta = (baseline as f64 - measured as f64) / baseline as f64 * 100.0;
    if delta >= 0.0 {
        format!("{delta:.2}% smaller")
    } else {
        format!("{:.2}% larger", -delta)
    }
}

fn describe_speed_delta(baseline: Duration, measured: Duration) -> String {
    let baseline = baseline.as_secs_f64();
    let measured = measured.as_secs_f64();

    if baseline == 0.0 || measured == 0.0 {
        return "timing difference was too small to compare".to_string();
    }

    if measured <= baseline {
        format!("{:.2}x faster", baseline / measured)
    } else {
        format!("{:.2}x slower", measured / baseline)
    }
}
