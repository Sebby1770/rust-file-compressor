use std::{
    fs::{self, File},
    io::{self, BufReader, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use indicatif::{ProgressBar, ProgressStyle};
use rust_file_compressor::{
    append_suffix, archive_tree, bytes_per_second, cat_member, check_seal, compress_bytes,
    compress_dir_recursive_ex, compress_file_dry_run, compress_file_opts, compress_to_path,
    decompress_dir_recursive_ex, decompress_file_opts, decompress_reader, decompress_to_path,
    default_decompressed_path, diff_archives, display_bytes, doctor, grep_archive,
    inspect_file, is_stdio_path, list_archive, pack_directory_opts, ratio_percent, repack_archive,
    resolve_threads, seal_archive, unpack_archive_opts, verify_file, ArchiveInfo, DiffResult,
    DiffStatus, IoStats, ListResult, PackInfo, PackOpts, Preset, UnpackOpts, DEFAULT_GREP_MAX_SIZE,
    DEFAULT_LEVEL,
};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "rzc",
    author,
    version,
    about = "Fast Rust file compression with pack archives, integrity checks, and zip benchmarks"
)]
struct Cli {
    /// Less chatty output (suppress progress bars and non-essential messages).
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

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

        /// Estimate compressed size without writing output.
        #[arg(long)]
        dry_run: bool,

        /// Glob patterns to exclude when using --recursive (repeatable).
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,

        /// Overwrite existing output files.
        #[arg(long)]
        force: bool,
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

        /// Skip writing if the output path already exists.
        #[arg(long)]
        skip_existing: bool,

        /// Overwrite existing output files (default behaviour when not skipping).
        #[arg(long)]
        force: bool,
    },

    /// Pack a directory into a multi-file v3 archive.
    Pack {
        /// Directory to pack.
        input: PathBuf,

        /// Output archive path (default: <dir>.rzst).
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

        /// Glob patterns to exclude (repeatable).
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,

        /// Only pack files modified within the last N days.
        #[arg(long = "newer-than", value_name = "DAYS")]
        newer_than: Option<u64>,

        /// Overwrite existing output archive.
        #[arg(long)]
        force: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Unpack a multi-file v3 archive into a directory.
    Unpack {
        /// Pack archive (.rzst v3).
        input: PathBuf,

        /// Output directory (default: strip .rzst or <input>.unpacked).
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Skip members whose output path already exists.
        #[arg(long)]
        skip_existing: bool,

        /// Extract only this archive member path.
        #[arg(long = "only", value_name = "PATH")]
        only: Option<String>,

        /// Strip N leading path components from member paths.
        #[arg(long = "strip-components", value_name = "N", default_value_t = 0)]
        strip_components: u32,

        /// Overwrite existing member files (required when a destination exists).
        #[arg(long)]
        force: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Write one archive member (or a single-file archive) to stdout.
    Cat {
        /// .rzst archive.
        archive: PathBuf,

        /// Member path inside a v3 pack (optional for single-file archives).
        member: Option<String>,
    },

    /// Compare two archives by member paths and checksums.
    Diff {
        /// First archive.
        archive_a: PathBuf,

        /// Second archive.
        archive_b: PathBuf,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,

        /// Exit 0 even when archives differ (default: exit 1 on differences).
        #[arg(long)]
        quiet: bool,
    },

    /// List members / metadata of a single-file or pack archive.
    List {
        /// .rzst file to list.
        input: PathBuf,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show container metadata without fully decompressing.
    Info {
        /// .rzst file to inspect.
        input: PathBuf,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Verify size and checksum by decompressing to a sink.
    Verify {
        /// .rzst file to verify.
        input: PathBuf,
    },

    /// Run self-tests (zstd + container roundtrips in memory / temp).
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Generate shell completion scripts.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },

    /// Print pack member paths as a directory tree (v3 packs).
    Tree {
        /// Pack archive (.rzst v3).
        archive: PathBuf,
    },

    /// Search decompressed text of archive members for a regex/substring.
    Grep {
        /// Regex pattern (Rust regex syntax; plain text works as a substring).
        pattern: String,

        /// .rzst archive (single-file or pack).
        archive: PathBuf,

        /// Skip members larger than this many bytes when decompressed (default: 32 MiB).
        #[arg(long = "max-size", default_value_t = DEFAULT_GREP_MAX_SIZE)]
        max_size: u64,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Write a SHA-256 integrity sidecar (`file.rzst.sha256`).
    Seal {
        /// .rzst file to seal.
        archive: PathBuf,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Verify an archive against its `.sha256` seal sidecar.
    Check {
        /// .rzst file to check.
        archive: PathBuf,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Rewrite a pack archive, dropping members that match exclude globs.
    Repack {
        /// Input pack archive (.rzst v3).
        input: PathBuf,

        /// Output pack path.
        #[arg(short, long)]
        output: PathBuf,

        /// Glob patterns of members to drop (repeatable).
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,

        /// Overwrite existing output archive.
        #[arg(long)]
        force: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug)]
struct TimedStats {
    output_bytes: u64,
    elapsed: Duration,
}

#[derive(Serialize)]
struct BenchJson {
    input: String,
    input_bytes: u64,
    level: i32,
    threads: u32,
    rzc_bytes: u64,
    rzc_ratio_percent: f64,
    rzc_seconds: f64,
    zip_bytes: u64,
    zip_ratio_percent: f64,
    zip_seconds: f64,
    summary: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let quiet = cli.quiet;

    match cli.command {
        Commands::Compress {
            input,
            output,
            level,
            preset,
            threads,
            recursive,
            dry_run,
            excludes,
            force,
        } => {
            let level = resolve_level(level, preset);
            let threads = resolve_threads(threads);
            run_compress(
                &input,
                output.as_deref(),
                level,
                threads,
                recursive,
                dry_run,
                &excludes,
                force,
                quiet,
            )?;
        }
        Commands::Decompress {
            input,
            output,
            recursive,
            skip_existing,
            force,
        } => {
            run_decompress(
                &input,
                output.as_deref(),
                recursive,
                skip_existing,
                force,
                quiet,
            )?;
        }
        Commands::Pack {
            input,
            output,
            level,
            preset,
            threads,
            excludes,
            newer_than,
            force,
            json,
        } => {
            let level = resolve_level(level, preset);
            let threads = resolve_threads(threads);
            run_pack(
                &input,
                output.as_deref(),
                level,
                threads,
                &excludes,
                newer_than,
                force,
                json,
                quiet,
            )?;
        }
        Commands::Unpack {
            input,
            output,
            skip_existing,
            only,
            strip_components,
            force,
            json,
        } => {
            run_unpack(
                &input,
                output.as_deref(),
                skip_existing,
                only.as_deref(),
                strip_components,
                force,
                json,
                quiet,
            )?;
        }
        Commands::Cat { archive, member } => {
            run_cat(&archive, member.as_deref(), quiet)?;
        }
        Commands::Diff {
            archive_a,
            archive_b,
            json,
            quiet: diff_quiet,
        } => {
            run_diff(&archive_a, &archive_b, json, diff_quiet || quiet)?;
        }
        Commands::List { input, json } => {
            run_list(&input, json)?;
        }
        Commands::Info { input, json } => {
            run_info(&input, json)?;
        }
        Commands::Verify { input } => {
            run_verify(&input, quiet)?;
        }
        Commands::Doctor { json } => {
            run_doctor(json)?;
        }
        Commands::Completions { shell } => {
            run_completions(shell)?;
        }
        Commands::Tree { archive } => {
            run_tree(&archive)?;
        }
        Commands::Grep {
            pattern,
            archive,
            max_size,
            json,
        } => {
            run_grep(&pattern, &archive, max_size, json)?;
        }
        Commands::Seal { archive, json } => {
            run_seal(&archive, json, quiet)?;
        }
        Commands::Check { archive, json } => {
            run_check(&archive, json, quiet)?;
        }
        Commands::Repack {
            input,
            output,
            excludes,
            force,
            json,
        } => {
            run_repack(&input, &output, &excludes, force, json, quiet)?;
        }
        Commands::Bench {
            input,
            level,
            threads,
            keep_artifacts,
            json,
        } => run_benchmark(
            &input,
            level,
            resolve_threads(threads),
            keep_artifacts,
            json,
            quiet,
        )?,
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

#[allow(clippy::too_many_arguments)]
fn run_compress(
    input: &Path,
    output: Option<&Path>,
    level: i32,
    threads: u32,
    recursive: bool,
    dry_run: bool,
    excludes: &[String],
    force: bool,
    quiet: bool,
) -> Result<()> {
    let _ = quiet;
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
        let results = compress_dir_recursive_ex(input, level, threads, excludes, dry_run, None)?;
        if results.is_empty() {
            println!("No files compressed under {}", input.display());
        } else {
            let verb = if dry_run {
                "Dry-run: would compress"
            } else {
                "Compressed"
            };
            println!("{verb} {} file(s) under {}", results.len(), input.display());
            print_ratio_table(&results);
        }
        return Ok(());
    }

    if !excludes.is_empty() {
        bail!("--exclude requires --recursive (or use `rzc pack --exclude`)");
    }

    if dry_run {
        if is_stdio_path(input) {
            let mut data = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut data)
                .context("reading stdin")?;
            let mut sink = rust_file_compressor::CountingSink::default();
            compress_bytes(&data, &mut sink, level, threads, None)?;
            println!("Dry-run (stdin)");
            println!(
                "Input:  {} ({})",
                data.len(),
                display_bytes(data.len() as u64)
            );
            println!(
                "Est. compressed: {} ({})",
                sink.bytes,
                display_bytes(sink.bytes)
            );
            println!(
                "Ratio:  {:.2}%",
                ratio_percent(sink.bytes, data.len() as u64)
            );
            return Ok(());
        }
        let dry = compress_file_dry_run(input, level, threads)?;
        println!("Dry-run: {}", dry.input_path.display());
        println!(
            "Input:  {} ({})",
            dry.input_bytes,
            display_bytes(dry.input_bytes)
        );
        println!(
            "Est. compressed: {} ({})",
            dry.estimated_compressed_bytes,
            display_bytes(dry.estimated_compressed_bytes)
        );
        println!("Ratio:  {:.2}%", dry.ratio_percent);
        return Ok(());
    }

    let out_is_stdio = output.map(is_stdio_path).unwrap_or(false);
    let in_is_stdio = is_stdio_path(input);

    if in_is_stdio && output.is_none() {
        compress_stdio_to_stdio(level, threads)?;
        return Ok(());
    }

    if in_is_stdio {
        let output = output.expect("checked above");
        if is_stdio_path(output) {
            compress_stdio_to_stdio(level, threads)?;
        } else {
            if output.exists() && !force {
                bail!(
                    "output already exists: {} (use --force to overwrite)",
                    output.display()
                );
            }
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
    let stats = compress_file_opts(input, &output_path, level, threads, force, progress_ref)?;
    let elapsed = start.elapsed();
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }
    print_compress_stats(&stats, elapsed, true);
    Ok(())
}

/// Print per-file rows plus a totals / ratio summary table.
fn print_ratio_table(results: &[IoStats]) {
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;

    println!(
        "{:<10} {:>12} {:>12} {:>8}  path",
        "status", "original", "compressed", "ratio"
    );
    for s in results {
        total_in += s.input_bytes;
        total_out += s.output_bytes;
        println!(
            "{:<10} {:>12} {:>12} {:>7.1}%  {}",
            "ok",
            display_bytes(s.input_bytes),
            display_bytes(s.output_bytes),
            ratio_percent(s.output_bytes, s.input_bytes),
            s.output_path.display()
        );
    }
    println!(
        "{:<10} {:>12} {:>12} {:>7.1}%  ({} files)",
        "TOTAL",
        display_bytes(total_in),
        display_bytes(total_out),
        ratio_percent(total_out, total_in),
        results.len()
    );
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

fn run_decompress(
    input: &Path,
    output: Option<&Path>,
    recursive: bool,
    skip_existing: bool,
    force: bool,
    quiet: bool,
) -> Result<()> {
    let _ = (force, quiet); // decompress overwrites unless --skip-existing
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
        let results = decompress_dir_recursive_ex(input, skip_existing, None)?;
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
        if skip_existing {
            bail!("--skip-existing is not supported with stdin");
        }
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
        if skip_existing {
            bail!("--skip-existing is not supported with stdout");
        }
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

    if skip_existing && output_path.exists() {
        println!("Skipped existing output: {}", output_path.display());
        return Ok(());
    }

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
    let stats = decompress_file_opts(input, &output_path, skip_existing, progress_ref)?;
    let elapsed = start.elapsed();
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }
    print_decompress_stats(&stats, elapsed, true);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_pack(
    input: &Path,
    output: Option<&Path>,
    level: i32,
    threads: u32,
    excludes: &[String],
    newer_than: Option<u64>,
    force: bool,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let _ = quiet;
    if !input.is_dir() {
        bail!("pack requires a directory; got {}", input.display());
    }
    let output_path = output.map(Path::to_path_buf).unwrap_or_else(|| {
        let name = input
            .file_name()
            .map(|n| PathBuf::from(n).with_extension("rzst"))
            .unwrap_or_else(|| PathBuf::from("archive.rzst"));
        if let Some(parent) = input.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join(name);
            }
        }
        name
    });

    // Count files first for a solid multi-file progress bar.
    let pb = if !json && io::stderr().is_terminal() {
        let bar = ProgressBar::new(0);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg} [{bar:40.cyan/blue}] {pos}/{len} files ({eta})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
        );
        bar.set_message("packing");
        Some(bar)
    } else {
        None
    };
    let progress = pb.as_ref().map(|bar| {
        let bar = bar.clone();
        move |done: u64, total: u64| {
            if total > 0 {
                bar.set_length(total);
            }
            bar.set_position(done);
        }
    });
    let progress_ref = progress
        .as_ref()
        .map(|f| f as &rust_file_compressor::ProgressFn<'_>);

    let opts = PackOpts {
        excludes: excludes.to_vec(),
        newer_than_days: newer_than,
        force,
    };
    let stats = pack_directory_opts(input, &output_path, level, threads, &opts, progress_ref)?;
    if let Some(bar) = pb {
        bar.finish_and_clear();
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Packed {} file(s) -> {}",
            stats.file_count,
            stats.output_path.display()
        );
        println!(
            "Original: {} | Archive: {} | Ratio: {:.2}%",
            display_bytes(stats.original_bytes),
            display_bytes(stats.archive_bytes),
            ratio_percent(stats.archive_bytes, stats.original_bytes)
        );
    }
    Ok(())
}

fn run_unpack(
    input: &Path,
    output: Option<&Path>,
    skip_existing: bool,
    only: Option<&str>,
    strip_components: u32,
    force: bool,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let _ = quiet;
    let output_dir = output.map(Path::to_path_buf).unwrap_or_else(|| {
        let text = input.to_string_lossy();
        if let Some(stripped) = text.strip_suffix(".rzst") {
            PathBuf::from(stripped)
        } else {
            append_suffix(input, ".unpacked")
        }
    });

    // Without --force, refuse to overwrite existing member files (use --skip-existing
    // to leave them alone, or --force to replace). New files always write fine.
    let opts = UnpackOpts {
        skip_existing,
        only: only.map(|s| s.to_string()),
        strip_components,
        force,
    };

    let stats = unpack_archive_opts(input, &output_dir, &opts)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Unpacked {} file(s) into {} (skipped {})",
            stats.written,
            stats.output_dir.display(),
            stats.skipped
        );
        for m in &stats.members {
            let tag = if m.skipped { "skip" } else { "ok  " };
            println!("  [{tag}] {} ({})", m.path, display_bytes(m.original_bytes));
        }
    }
    Ok(())
}

fn run_cat(archive: &Path, member: Option<&str>, quiet: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let written = cat_member(archive, member, &mut handle)?;
    // Only log to stderr so stdout stays clean for piping.
    if !quiet && io::stderr().is_terminal() {
        eprintln!("Wrote {} to stdout", display_bytes(written));
    }
    Ok(())
}

fn run_diff(a: &Path, b: &Path, json: bool, quiet: bool) -> Result<()> {
    let result = diff_archives(a, b)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_diff(&result);
    }
    if !result.is_equal() && !quiet {
        std::process::exit(1);
    }
    Ok(())
}

fn print_diff(result: &DiffResult) {
    println!(
        "Comparing:\n  A: {}\n  B: {}",
        result.archive_a.display(),
        result.archive_b.display()
    );
    println!();
    if result.is_equal() {
        println!(
            "Archives are identical ({} member(s))",
            result.identical
        );
        return;
    }
    println!(
        "{:<10} {:<40} detail",
        "status", "path"
    );
    for e in &result.entries {
        match e.status {
            DiffStatus::Identical => continue,
            DiffStatus::OnlyInA => {
                println!("{:<10} {:<40} only in A", "only_a", truncate_display(&e.path, 40));
            }
            DiffStatus::OnlyInB => {
                println!("{:<10} {:<40} only in B", "only_b", truncate_display(&e.path, 40));
            }
            DiffStatus::Changed => {
                println!(
                    "{:<10} {:<40} checksum differs",
                    "changed",
                    truncate_display(&e.path, 40)
                );
            }
        }
    }
    println!();
    println!(
        "Summary: {} identical, {} changed, {} only in A, {} only in B",
        result.identical, result.changed, result.only_in_a, result.only_in_b
    );
}

fn run_list(input: &Path, json: bool) -> Result<()> {
    let result = list_archive(input)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    match result {
        ListResult::Single(info) => {
            println!("Kind:            single-file");
            print_archive_info(&info);
        }
        ListResult::Pack(info) => {
            print_pack_info(&info);
        }
    }
    Ok(())
}

fn run_info(input: &Path, json: bool) -> Result<()> {
    let result = list_archive(input)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    match result {
        ListResult::Single(info) => print_archive_info(&info),
        ListResult::Pack(info) => print_pack_info(&info),
    }
    Ok(())
}

fn print_archive_info(info: &ArchiveInfo) {
    let header = &info.header;
    println!("File:            {}", info.path.display());
    println!("Kind:            single-file");
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

fn print_pack_info(info: &PackInfo) {
    println!("File:            {}", info.path.display());
    println!("Kind:            pack (multi-file)");
    println!("Magic:           RZC1");
    println!("Version:         {}", info.version);
    println!("Level:           {}", info.level);
    println!("Members:         {}", info.file_count);
    println!(
        "Archive size:    {} ({})",
        info.archive_size,
        display_bytes(info.archive_size)
    );
    println!(
        "Total original:  {} ({})",
        info.total_original_len(),
        display_bytes(info.total_original_len())
    );
    println!("Ratio:           {:.2}%", info.ratio_percent());
    println!();
    println!(
        "{:<40} {:>12} {:>12} {:>8}",
        "path", "original", "compressed", "ratio"
    );
    for m in &info.members {
        println!(
            "{:<40} {:>12} {:>12} {:>7.1}%",
            truncate_display(&m.path, 40),
            display_bytes(m.original_len),
            display_bytes(m.compressed_len),
            m.ratio_percent()
        );
    }
}

fn truncate_display(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn run_verify(input: &Path, quiet: bool) -> Result<()> {
    let _ = quiet;
    let result = list_archive(input).ok();
    let total = match &result {
        Some(ListResult::Single(i)) => i.header.original_len,
        Some(ListResult::Pack(p)) => p.total_original_len(),
        None => 0,
    };
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
    match result {
        Some(ListResult::Single(i)) if i.header.has_checksum() => {
            println!("Checksum: valid (SHA-256)");
        }
        Some(ListResult::Single(_)) => {
            println!("Checksum: not present (v1); size check passed");
        }
        Some(ListResult::Pack(p)) => {
            println!("Pack: {} members, all checksums valid", p.file_count);
        }
        None => {}
    }
    Ok(())
}

fn run_doctor(json: bool) -> Result<()> {
    let report = doctor();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("rzc doctor");
        for msg in &report.messages {
            println!("  {msg}");
        }
        if report.ok {
            println!("Status: OK");
        } else {
            println!("Status: FAILED");
        }
    }
    if !report.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn run_tree(archive: &Path) -> Result<()> {
    let tree = archive_tree(archive)?;
    print!("{tree}");
    if !tree.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn run_grep(pattern: &str, archive: &Path, max_size: u64, json: bool) -> Result<()> {
    let result = grep_archive(archive, pattern, max_size)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.matches.is_empty() {
        println!(
            "No matches for {pattern:?} in {} (scanned {}, skipped {})",
            archive.display(),
            result.members_searched,
            result.members_skipped
        );
    } else {
        for m in &result.matches {
            println!("{}:{}:{}", m.member, m.line_number, m.line);
        }
        eprintln!(
            "({} match(es); scanned {}, skipped large/binary {})",
            result.matches.len(),
            result.members_searched,
            result.members_skipped
        );
    }
    Ok(())
}

fn run_seal(archive: &Path, json: bool, quiet: bool) -> Result<()> {
    let info = seal_archive(archive)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else if !quiet {
        println!(
            "Sealed {} → {} ({})",
            info.archive.display(),
            info.sidecar.display(),
            &info.sha256[..16]
        );
    }
    Ok(())
}

fn run_check(archive: &Path, json: bool, quiet: bool) -> Result<()> {
    let info = check_seal(archive)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else if !quiet {
        println!(
            "OK {} matches seal {} ({})",
            info.archive.display(),
            info.sidecar.display(),
            &info.sha256[..16]
        );
    }
    Ok(())
}

fn run_repack(
    input: &Path,
    output: &Path,
    excludes: &[String],
    force: bool,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let stats = repack_archive(input, output, excludes, force)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else if !quiet {
        println!(
            "Repacked {} → {} (kept {}, excluded {})",
            input.display(),
            output.display(),
            stats.kept,
            stats.excluded
        );
    }
    Ok(())
}

fn run_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut io::stdout());
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

fn run_benchmark(
    input: &Path,
    level: i32,
    threads: u32,
    keep_artifacts: bool,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let _ = quiet;
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

    let size_delta = describe_size_delta(zip.output_bytes, rzc.output_bytes);
    let speed_delta = describe_speed_delta(zip.elapsed, rzc.elapsed);
    let summary = format!("rzc output was {size_delta} than zip -9 and {speed_delta}.");

    if json {
        let payload = BenchJson {
            input: input.display().to_string(),
            input_bytes,
            level,
            threads,
            rzc_bytes: rzc.output_bytes,
            rzc_ratio_percent: ratio_percent(rzc.output_bytes, input_bytes),
            rzc_seconds: rzc.elapsed.as_secs_f64(),
            zip_bytes: zip.output_bytes,
            zip_ratio_percent: ratio_percent(zip.output_bytes, input_bytes),
            zip_seconds: zip.elapsed.as_secs_f64(),
            summary: summary.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
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
        println!("Result: {summary}");
    }

    if !keep_artifacts {
        fs::remove_dir_all(&bench_dir)
            .with_context(|| format!("removing benchmark directory {}", bench_dir.display()))?;
    }

    Ok(())
}

fn timed_compress(input: &Path, output: &Path, level: i32, threads: u32) -> Result<TimedStats> {
    let start = Instant::now();
    let stats = compress_file_opts(input, output, level, threads, true, None)?;
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
