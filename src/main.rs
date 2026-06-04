use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

const MAGIC: &[u8; 4] = b"RZC1";
const VERSION: u8 = 1;

#[derive(Parser)]
#[command(
    name = "rzc",
    author,
    version,
    about = "Fast Rust file compression with reproducible zip benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compress a file to the .rzst container format.
    Compress {
        /// File to compress.
        input: PathBuf,

        /// Output path. Defaults to <input>.rzst.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// zstd compression level. 12 is tuned to beat zip -9 on the included text benchmark.
        #[arg(short, long, default_value_t = 12, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: i32,

        /// Worker threads. 0 uses the available CPU count, capped at 8.
        #[arg(short, long, default_value_t = 0)]
        threads: u32,
    },

    /// Decompress a .rzst file.
    Decompress {
        /// File to decompress.
        input: PathBuf,

        /// Output path. Defaults to removing .rzst, or <input>.out.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Compare this compressor against zip -9 on one input file.
    Bench {
        /// Text file to benchmark.
        input: PathBuf,

        /// zstd compression level for this compressor.
        #[arg(short, long, default_value_t = 12, value_parser = clap::value_parser!(i32).range(1..=22))]
        level: i32,

        /// Worker threads. 0 uses the available CPU count, capped at 8.
        #[arg(short, long, default_value_t = 0)]
        threads: u32,

        /// Keep generated .rzst and .zip files in a temporary benchmark folder.
        #[arg(long)]
        keep_artifacts: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct Header {
    level: i32,
    original_len: u64,
}

#[derive(Debug)]
struct FileStats {
    input_bytes: u64,
    output_bytes: u64,
    elapsed: Duration,
    output_path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Compress {
            input,
            output,
            level,
            threads,
        } => {
            let output = output.unwrap_or_else(|| append_suffix(&input, ".rzst"));
            let threads = resolve_threads(threads);
            let stats = timed_compress(&input, &output, level, threads)?;
            println!(
                "Compressed {} -> {}",
                display_bytes(stats.input_bytes),
                display_bytes(stats.output_bytes)
            );
            println!("Output: {}", stats.output_path.display());
            println!(
                "Ratio: {:.2}% | Time: {:.3}s | Throughput: {}/s",
                ratio_percent(stats.output_bytes, stats.input_bytes),
                stats.elapsed.as_secs_f64(),
                display_bytes(bytes_per_second(stats.input_bytes, stats.elapsed))
            );
        }
        Commands::Decompress { input, output } => {
            let output = output.unwrap_or_else(|| default_decompressed_path(&input));
            let stats = timed_decompress(&input, &output)?;
            println!(
                "Decompressed {} -> {}",
                display_bytes(stats.input_bytes),
                display_bytes(stats.output_bytes)
            );
            println!("Output: {}", stats.output_path.display());
            println!(
                "Time: {:.3}s | Throughput: {}/s",
                stats.elapsed.as_secs_f64(),
                display_bytes(bytes_per_second(stats.output_bytes, stats.elapsed))
            );
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

fn timed_compress(input: &Path, output: &Path, level: i32, threads: u32) -> Result<FileStats> {
    let start = Instant::now();
    compress_file(input, output, level, threads)?;
    let elapsed = start.elapsed();

    Ok(FileStats {
        input_bytes: fs::metadata(input)
            .with_context(|| format!("reading metadata for {}", input.display()))?
            .len(),
        output_bytes: fs::metadata(output)
            .with_context(|| format!("reading metadata for {}", output.display()))?
            .len(),
        elapsed,
        output_path: output.to_path_buf(),
    })
}

fn timed_decompress(input: &Path, output: &Path) -> Result<FileStats> {
    let start = Instant::now();
    decompress_file(input, output)?;
    let elapsed = start.elapsed();

    Ok(FileStats {
        input_bytes: fs::metadata(input)
            .with_context(|| format!("reading metadata for {}", input.display()))?
            .len(),
        output_bytes: fs::metadata(output)
            .with_context(|| format!("reading metadata for {}", output.display()))?
            .len(),
        elapsed,
        output_path: output.to_path_buf(),
    })
}

fn compress_file(input: &Path, output: &Path, level: i32, threads: u32) -> Result<()> {
    let input_file =
        File::open(input).with_context(|| format!("opening input {}", input.display()))?;
    let input_len = input_file
        .metadata()
        .with_context(|| format!("reading metadata for {}", input.display()))?
        .len();

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let mut reader = BufReader::new(input_file);
    let mut writer = BufWriter::new(
        File::create(output).with_context(|| format!("creating output {}", output.display()))?,
    );

    write_header(
        &mut writer,
        Header {
            level,
            original_len: input_len,
        },
    )?;

    let mut encoder = zstd::stream::Encoder::new(writer, level)
        .with_context(|| format!("creating zstd encoder at level {level}"))?;
    if threads > 1 {
        encoder
            .multithread(threads)
            .with_context(|| format!("enabling {threads} zstd worker threads"))?;
    }

    io::copy(&mut reader, &mut encoder).context("compressing data")?;
    let mut writer = encoder.finish().context("finishing zstd frame")?;
    writer.flush().context("flushing compressed output")?;
    Ok(())
}

fn decompress_file(input: &Path, output: &Path) -> Result<()> {
    let input_file = File::open(input)
        .with_context(|| format!("opening compressed input {}", input.display()))?;
    let mut reader = BufReader::new(input_file);
    let header = read_header(&mut reader)?;

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let mut decoder = zstd::stream::Decoder::new(reader).context("creating zstd decoder")?;
    let mut writer = BufWriter::new(
        File::create(output).with_context(|| format!("creating output {}", output.display()))?,
    );

    let written = io::copy(&mut decoder, &mut writer).context("decompressing data")?;
    writer.flush().context("flushing decompressed output")?;

    if written != header.original_len {
        bail!(
            "decompressed size mismatch: expected {}, wrote {}",
            header.original_len,
            written
        );
    }

    Ok(())
}

fn write_header(mut writer: impl Write, header: Header) -> Result<()> {
    writer.write_all(MAGIC)?;
    writer.write_all(&[VERSION])?;
    writer.write_all(&header.level.to_le_bytes())?;
    writer.write_all(&header.original_len.to_le_bytes())?;
    Ok(())
}

fn read_header(mut reader: impl Read) -> Result<Header> {
    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .context("reading compressor magic")?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }

    let mut version = [0_u8; 1];
    reader
        .read_exact(&mut version)
        .context("reading container version")?;
    if version[0] != VERSION {
        bail!(
            "unsupported container version {}; expected {}",
            version[0],
            VERSION
        );
    }

    let mut level = [0_u8; 4];
    reader
        .read_exact(&mut level)
        .context("reading compression level")?;

    let mut original_len = [0_u8; 8];
    reader
        .read_exact(&mut original_len)
        .context("reading original file length")?;

    Ok(Header {
        level: i32::from_le_bytes(level),
        original_len: u64::from_le_bytes(original_len),
    })
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

fn timed_zip(input: &Path, output: &Path) -> Result<FileStats> {
    let input_bytes = fs::metadata(input)
        .with_context(|| format!("reading metadata for {}", input.display()))?
        .len();
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

    Ok(FileStats {
        input_bytes,
        output_bytes: fs::metadata(output)
            .with_context(|| format!("reading metadata for {}", output.display()))?
            .len(),
        elapsed,
        output_path: output.to_path_buf(),
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

fn resolve_threads(requested: u32) -> u32 {
    if requested > 0 {
        return requested;
    }

    std::thread::available_parallelism()
        .map(|count| count.get().min(8) as u32)
        .unwrap_or(1)
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn default_decompressed_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_suffix(".rzst") {
        PathBuf::from(stripped)
    } else {
        append_suffix(path, ".out")
    }
}

fn benchmark_dir() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    std::env::temp_dir().join(format!("rzc-bench-{}-{millis}", std::process::id()))
}

fn display_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

fn ratio_percent(compressed: u64, original: u64) -> f64 {
    if original == 0 {
        0.0
    } else {
        compressed as f64 / original as f64 * 100.0
    }
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

fn bytes_per_second(bytes: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        bytes
    } else {
        (bytes as f64 / seconds) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips_text_file() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("sample.txt");
        let compressed = temp.path().join("sample.txt.rzst");
        let output = temp.path().join("sample.out.txt");
        let text = "Rust compression loves repeated natural language.\n".repeat(8_192);

        fs::write(&input, text.as_bytes())?;
        compress_file(&input, &compressed, 3, 1)?;
        decompress_file(&compressed, &output)?;

        assert_eq!(fs::read(&input)?, fs::read(&output)?);
        assert!(fs::metadata(&compressed)?.len() < fs::metadata(&input)?.len());
        Ok(())
    }

    #[test]
    fn rejects_unknown_container() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("not-rzc.bin");
        let output = temp.path().join("out.txt");

        fs::write(&input, b"not a compressed rzc file")?;
        let error = decompress_file(&input, &output).expect_err("invalid magic should fail");

        assert!(error.to_string().contains("invalid file magic"));
        Ok(())
    }

    #[test]
    fn default_paths_are_predictable() {
        assert_eq!(
            append_suffix(Path::new("notes.txt"), ".rzst"),
            PathBuf::from("notes.txt.rzst")
        );
        assert_eq!(
            default_decompressed_path(Path::new("notes.txt.rzst")),
            PathBuf::from("notes.txt")
        );
        assert_eq!(
            default_decompressed_path(Path::new("archive.bin")),
            PathBuf::from("archive.bin.out")
        );
    }
}
