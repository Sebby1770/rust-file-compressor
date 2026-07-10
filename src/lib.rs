//! Core library for the `rzc` file compressor.
//!
//! Provides the `.rzst` container format (RZC1), zstd compress/decompress,
//! integrity checking (SHA-256 in format v2), and helpers for CLI tooling.

use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Cursor, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

/// Container magic bytes. Kept as `RZC1` across format versions.
pub const MAGIC: &[u8; 4] = b"RZC1";

/// Current container version (includes SHA-256 of original payload).
pub const VERSION: u8 = 2;

/// Legacy container version without a checksum.
pub const VERSION_V1: u8 = 1;

/// Length of the SHA-256 digest stored in v2 headers.
pub const HASH_LEN: usize = 32;

/// Default zstd level for the balanced preset / CLI default.
pub const DEFAULT_LEVEL: i32 = 12;

/// Compression quality presets mapped to zstd levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Fast,
    Balanced,
    Max,
}

impl Preset {
    /// zstd compression level for this preset.
    pub fn level(self) -> i32 {
        match self {
            Preset::Fast => 3,
            Preset::Balanced => 12,
            Preset::Max => 19,
        }
    }

    /// Parse from a CLI string (`fast`, `balanced`, `max`).
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fast" => Ok(Preset::Fast),
            "balanced" => Ok(Preset::Balanced),
            "max" => Ok(Preset::Max),
            other => bail!("unknown preset '{other}'; expected fast|balanced|max"),
        }
    }
}

/// Parsed `.rzst` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Container version (`1` or `2`).
    pub version: u8,
    /// zstd compression level used at compress time.
    pub level: i32,
    /// Original uncompressed payload size in bytes.
    pub original_len: u64,
    /// SHA-256 of the original payload when `version >= 2`.
    pub checksum: Option<[u8; HASH_LEN]>,
}

impl Header {
    /// Byte length of this header on disk.
    pub fn encoded_len(&self) -> usize {
        // magic(4) + version(1) + level(4) + original_len(8) [+ hash(32)]
        let base = 4 + 1 + 4 + 8;
        if self.checksum.is_some() {
            base + HASH_LEN
        } else {
            base
        }
    }

    /// Whether this header carries a SHA-256 checksum.
    pub fn has_checksum(&self) -> bool {
        self.checksum.is_some()
    }
}

/// Summary of a compressed archive (for `info` and programmatic inspection).
#[derive(Debug, Clone)]
pub struct ArchiveInfo {
    pub header: Header,
    pub compressed_size: u64,
    pub path: PathBuf,
}

impl ArchiveInfo {
    /// Compression ratio as a percentage of original size.
    pub fn ratio_percent(&self) -> f64 {
        ratio_percent(self.compressed_size, self.header.original_len)
    }
}

/// Result statistics for a compress/decompress operation.
#[derive(Debug, Clone)]
pub struct IoStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub output_path: PathBuf,
}

/// Optional progress callback: `(bytes_processed, total_hint)`.
/// `total_hint` may be 0 when unknown.
pub type ProgressFn<'a> = dyn Fn(u64, u64) + 'a;

// ---------------------------------------------------------------------------
// Header I/O
// ---------------------------------------------------------------------------

/// Write a v2 header (with checksum) or a legacy-shaped header depending on
/// whether `header.checksum` is set. Prefer [`write_header_v2`].
pub fn write_header(mut writer: impl Write, header: &Header) -> Result<()> {
    writer.write_all(MAGIC)?;
    writer.write_all(&[header.version])?;
    writer.write_all(&header.level.to_le_bytes())?;
    writer.write_all(&header.original_len.to_le_bytes())?;
    if let Some(hash) = &header.checksum {
        if header.version < VERSION {
            bail!("checksum present but version is {}", header.version);
        }
        writer.write_all(hash)?;
    } else if header.version >= VERSION {
        bail!("version {} requires a checksum", header.version);
    }
    Ok(())
}

/// Write a current (v2) header with the given fields.
pub fn write_header_v2(
    writer: impl Write,
    level: i32,
    original_len: u64,
    checksum: [u8; HASH_LEN],
) -> Result<()> {
    write_header(
        writer,
        &Header {
            version: VERSION,
            level,
            original_len,
            checksum: Some(checksum),
        },
    )
}

/// Read and validate an RZC1 header (v1 or v2).
pub fn read_header(mut reader: impl Read) -> Result<Header> {
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
    let version = version[0];
    if version != VERSION_V1 && version != VERSION {
        bail!(
            "unsupported container version {version}; supported versions are {VERSION_V1} and {VERSION}"
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

    let checksum = if version >= VERSION {
        let mut hash = [0_u8; HASH_LEN];
        reader
            .read_exact(&mut hash)
            .context("reading SHA-256 checksum")?;
        Some(hash)
    } else {
        None
    };

    Ok(Header {
        version,
        level: i32::from_le_bytes(level),
        original_len: u64::from_le_bytes(original_len),
        checksum,
    })
}

// ---------------------------------------------------------------------------
// Compress / decompress (streaming over Read/Write)
// ---------------------------------------------------------------------------

/// Compress `reader` into `writer` using the v2 container format.
///
/// The full input is hashed with SHA-256 while being compressed. Because the
/// header must record `original_len` and the checksum up front, the input is
/// fully read into memory first. For very large inputs prefer
/// [`compress_file`], which streams from disk after a size probe when possible.
///
/// This function always buffers the full input so the hash and length are known
/// before writing the header (required by the format).
pub fn compress_reader(
    mut reader: impl Read,
    writer: impl Write,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<u64> {
    let mut data = Vec::new();
    reader
        .read_to_end(&mut data)
        .context("reading input for compression")?;
    compress_bytes(&data, writer, level, threads, progress)
}

/// Compress an in-memory buffer into the v2 `.rzst` format.
pub fn compress_bytes(
    data: &[u8],
    mut writer: impl Write,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<u64> {
    let original_len = data.len() as u64;
    let checksum = sha256(data);

    write_header_v2(&mut writer, level, original_len, checksum)?;

    let mut encoder = zstd::stream::Encoder::new(writer, level)
        .with_context(|| format!("creating zstd encoder at level {level}"))?;
    if threads > 1 {
        encoder
            .multithread(threads)
            .with_context(|| format!("enabling {threads} zstd worker threads"))?;
    }

    copy_with_progress(&mut Cursor::new(data), &mut encoder, original_len, progress)
        .context("compressing data")?;
    let mut writer = encoder.finish().context("finishing zstd frame")?;
    writer.flush().context("flushing compressed output")?;
    Ok(original_len)
}

/// Compress a filesystem path to another path.
pub fn compress_file(
    input: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    let data = fs::read(input).with_context(|| format!("reading input {}", input.display()))?;
    let input_bytes = data.len() as u64;

    ensure_parent_dir(output)?;

    let out_file =
        File::create(output).with_context(|| format!("creating output {}", output.display()))?;
    let writer = BufWriter::new(out_file);
    compress_bytes(&data, writer, level, threads, progress)?;

    let output_bytes = fs::metadata(output)
        .with_context(|| format!("reading metadata for {}", output.display()))?
        .len();

    Ok(IoStats {
        input_bytes,
        output_bytes,
        output_path: output.to_path_buf(),
    })
}

/// Compress from a `Read` that may already be buffered (e.g. stdin).
/// Fully loads the input into memory for correct v2 headers.
pub fn compress_to_path(
    mut reader: impl Read,
    output: &Path,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    let mut data = Vec::new();
    reader
        .read_to_end(&mut data)
        .context("reading input for compression")?;
    let input_bytes = data.len() as u64;

    ensure_parent_dir(output)?;
    let writer = BufWriter::new(
        File::create(output).with_context(|| format!("creating output {}", output.display()))?,
    );
    compress_bytes(&data, writer, level, threads, progress)?;

    let output_bytes = fs::metadata(output)
        .with_context(|| format!("reading metadata for {}", output.display()))?
        .len();

    Ok(IoStats {
        input_bytes,
        output_bytes,
        output_path: output.to_path_buf(),
    })
}

/// Decompress a `.rzst` stream into `writer`, verifying size and checksum.
///
/// Returns the number of decompressed bytes written.
pub fn decompress_reader(
    mut reader: impl Read,
    mut writer: impl Write,
    progress: Option<&ProgressFn<'_>>,
) -> Result<u64> {
    let header = read_header(&mut reader)?;
    let mut decoder = zstd::stream::Decoder::new(reader).context("creating zstd decoder")?;

    // Hash while decompressing so we can verify integrity without a second pass.
    let mut hasher = Sha256::new();
    let total_hint = header.original_len;
    let mut written: u64 = 0;
    let mut buf = [0_u8; 64 * 1024];

    loop {
        let n = decoder.read(&mut buf).context("decompressing data")?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .context("writing decompressed data")?;
        hasher.update(&buf[..n]);
        written += n as u64;
        if let Some(cb) = progress {
            cb(written, total_hint);
        }
    }
    writer.flush().context("flushing decompressed output")?;

    // original_len == 0 is treated as "unknown" only for empty files; empty is valid.
    // We always enforce the recorded length when present (including 0).
    if written != header.original_len {
        bail!(
            "decompressed size mismatch: expected {}, wrote {}",
            header.original_len,
            written
        );
    }

    if let Some(expected) = header.checksum {
        let actual: [u8; HASH_LEN] = hasher.finalize().into();
        if actual != expected {
            bail!(
                "checksum mismatch: expected {}, got {}",
                hex::encode(expected),
                hex::encode(actual)
            );
        }
    }

    Ok(written)
}

/// Decompress a filesystem path to another path.
pub fn decompress_file(
    input: &Path,
    output: &Path,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    let input_file = File::open(input)
        .with_context(|| format!("opening compressed input {}", input.display()))?;
    let input_bytes = input_file
        .metadata()
        .with_context(|| format!("reading metadata for {}", input.display()))?
        .len();
    let reader = BufReader::new(input_file);

    ensure_parent_dir(output)?;
    let out_file =
        File::create(output).with_context(|| format!("creating output {}", output.display()))?;
    let writer = BufWriter::new(out_file);

    let output_bytes = decompress_reader(reader, writer, progress)?;

    Ok(IoStats {
        input_bytes,
        output_bytes,
        output_path: output.to_path_buf(),
    })
}

/// Decompress a `Read` into a path.
pub fn decompress_to_path(
    reader: impl Read,
    output: &Path,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    ensure_parent_dir(output)?;
    let writer = BufWriter::new(
        File::create(output).with_context(|| format!("creating output {}", output.display()))?,
    );
    // input_bytes unknown for pure streams
    let output_bytes = decompress_reader(reader, writer, progress)?;
    Ok(IoStats {
        input_bytes: 0,
        output_bytes,
        output_path: output.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// Info / verify
// ---------------------------------------------------------------------------

/// Parse archive metadata without fully decompressing.
pub fn inspect_file(path: &Path) -> Result<ArchiveInfo> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let compressed_size = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;
    Ok(ArchiveInfo {
        header,
        compressed_size,
        path: path.to_path_buf(),
    })
}

/// Decompress to a sink, verifying original size and checksum.
///
/// Returns the verified original byte count.
pub fn verify_file(path: &Path, progress: Option<&ProgressFn<'_>>) -> Result<u64> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    decompress_reader(reader, io::sink(), progress)
}

// ---------------------------------------------------------------------------
// Recursive directory helpers
// ---------------------------------------------------------------------------

/// Compress every regular file under `dir` into a sibling `.rzst` next to it.
/// Skips files that already end with `.rzst`.
pub fn compress_dir_recursive(
    dir: &Path,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<Vec<IoStats>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut results = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("rzst"))
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".rzst"))
        {
            continue;
        }
        let output = append_suffix(path, ".rzst");
        let stats = compress_file(path, &output, level, threads, progress)?;
        results.push(stats);
    }
    Ok(results)
}

/// Decompress every `.rzst` file under `dir` to the default output path.
pub fn decompress_dir_recursive(
    dir: &Path,
    progress: Option<&ProgressFn<'_>>,
) -> Result<Vec<IoStats>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut results = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_rzst = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".rzst"));
        if !is_rzst {
            continue;
        }
        let output = default_decompressed_path(path);
        let stats = decompress_file(path, &output, progress)?;
        results.push(stats);
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Utils
// ---------------------------------------------------------------------------

/// Compute SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; HASH_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Append a suffix to a path (e.g. `.rzst`).
pub fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Default output path when decompressing: strip `.rzst`, else append `.out`.
pub fn default_decompressed_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_suffix(".rzst") {
        PathBuf::from(stripped)
    } else {
        append_suffix(path, ".out")
    }
}

/// Resolve worker thread count: `0` means available CPUs capped at 8.
pub fn resolve_threads(requested: u32) -> u32 {
    if requested > 0 {
        return requested;
    }

    std::thread::available_parallelism()
        .map(|count| count.get().min(8) as u32)
        .unwrap_or(1)
}

/// Human-readable byte size (binary units).
pub fn display_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
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

/// Compressed / original * 100.
pub fn ratio_percent(compressed: u64, original: u64) -> f64 {
    if original == 0 {
        0.0
    } else {
        compressed as f64 / original as f64 * 100.0
    }
}

/// Bytes per second given elapsed time.
pub fn bytes_per_second(bytes: u64, elapsed: std::time::Duration) -> u64 {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        bytes
    } else {
        (bytes as f64 / seconds) as u64
    }
}

/// Whether `path` should be treated as stdin/stdout (`-`).
pub fn is_stdio_path(path: &Path) -> bool {
    path.as_os_str() == "-"
}

fn ensure_parent_dir(output: &Path) -> Result<()> {
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    Ok(())
}

fn copy_with_progress(
    reader: &mut impl Read,
    writer: &mut impl Write,
    total_hint: u64,
    progress: Option<&ProgressFn<'_>>,
) -> Result<u64> {
    let mut buf = [0_u8; 64 * 1024];
    let mut written = 0_u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        written += n as u64;
        if let Some(cb) = progress {
            cb(written, total_hint);
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn sample_text() -> String {
        "Rust compression loves repeated natural language.\n".repeat(8_192)
    }

    #[test]
    fn v2_round_trip_with_hash() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("sample.txt");
        let compressed = temp.path().join("sample.txt.rzst");
        let output = temp.path().join("sample.out.txt");
        let text = sample_text();

        fs::write(&input, text.as_bytes())?;
        compress_file(&input, &compressed, 3, 1, None)?;
        decompress_file(&compressed, &output, None)?;

        assert_eq!(fs::read(&input)?, fs::read(&output)?);
        assert!(fs::metadata(&compressed)?.len() < fs::metadata(&input)?.len());

        let info = inspect_file(&compressed)?;
        assert_eq!(info.header.version, VERSION);
        assert!(info.header.has_checksum());
        assert_eq!(info.header.original_len, text.len() as u64);
        assert_eq!(info.header.checksum, Some(sha256(text.as_bytes())));
        Ok(())
    }

    #[test]
    fn v1_round_trip_backward_compat() -> Result<()> {
        let text = sample_text();
        let mut container = Vec::new();

        // Manually write a v1 container (no checksum).
        write_header(
            &mut container,
            &Header {
                version: VERSION_V1,
                level: 3,
                original_len: text.len() as u64,
                checksum: None,
            },
        )?;
        let mut encoder = zstd::stream::Encoder::new(&mut container, 3)?;
        encoder.write_all(text.as_bytes())?;
        encoder.finish()?;

        let mut out = Vec::new();
        decompress_reader(Cursor::new(&container), &mut out, None)?;
        assert_eq!(out, text.as_bytes());

        let header = read_header(Cursor::new(&container))?;
        assert_eq!(header.version, VERSION_V1);
        assert!(!header.has_checksum());
        Ok(())
    }

    #[test]
    fn bad_checksum_fails() -> Result<()> {
        let text = b"hello integrity check";
        let mut good = Vec::new();
        compress_bytes(text, &mut good, 3, 1, None)?;

        // Flip a byte in the stored checksum (after magic+ver+level+len).
        // Layout: 4 + 1 + 4 + 8 = 17, then 32-byte hash.
        let mut bad = good.clone();
        let hash_offset = 4 + 1 + 4 + 8;
        bad[hash_offset] ^= 0xff;

        let err = decompress_reader(Cursor::new(&bad), io::sink(), None)
            .expect_err("tampered checksum should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "unexpected error: {msg}");
        Ok(())
    }

    #[test]
    fn rejects_unknown_container() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("not-rzc.bin");
        let output = temp.path().join("out.txt");

        fs::write(&input, b"not a compressed rzc file")?;
        let error = decompress_file(&input, &output, None).expect_err("invalid magic should fail");

        assert!(error.to_string().contains("invalid file magic"));
        Ok(())
    }

    #[test]
    fn info_parsing() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("data.bin");
        let compressed = temp.path().join("data.bin.rzst");
        fs::write(&input, b"aaaaaaaaaaaaaaaaaaaaaaaa")?;
        compress_file(&input, &compressed, 5, 1, None)?;

        let info = inspect_file(&compressed)?;
        assert_eq!(info.header.version, 2);
        assert_eq!(info.header.level, 5);
        assert_eq!(info.header.original_len, 24);
        assert!(info.header.has_checksum());
        assert!(info.compressed_size > 0);
        assert!(info.ratio_percent() > 0.0);
        Ok(())
    }

    #[test]
    fn verify_ok_and_fail() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("ok.txt");
        let compressed = temp.path().join("ok.txt.rzst");
        fs::write(&input, b"verify me please")?;
        compress_file(&input, &compressed, 3, 1, None)?;

        let n = verify_file(&compressed, None)?;
        assert_eq!(n, b"verify me please".len() as u64);

        // Corrupt payload after header.
        let mut bytes = fs::read(&compressed)?;
        if let Some(last) = bytes.last_mut() {
            *last ^= 0x55;
        }
        let bad = temp.path().join("bad.rzst");
        fs::write(&bad, &bytes)?;
        assert!(verify_file(&bad, None).is_err());
        Ok(())
    }

    #[test]
    fn recursive_dir_compress_decompress() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("sub"))?;
        fs::write(root.join("a.txt"), b"file a contents repeated ".repeat(50))?;
        fs::write(
            root.join("sub/b.txt"),
            b"file b contents repeated ".repeat(50),
        )?;

        let stats = compress_dir_recursive(&root, 3, 1, None)?;
        assert_eq!(stats.len(), 2);
        assert!(root.join("a.txt.rzst").is_file());
        assert!(root.join("sub/b.txt.rzst").is_file());

        // Remove originals and decompress.
        fs::remove_file(root.join("a.txt"))?;
        fs::remove_file(root.join("sub/b.txt"))?;

        let stats = decompress_dir_recursive(&root, None)?;
        assert_eq!(stats.len(), 2);
        assert_eq!(
            fs::read(root.join("a.txt"))?,
            b"file a contents repeated ".repeat(50)
        );
        assert_eq!(
            fs::read(root.join("sub/b.txt"))?,
            b"file b contents repeated ".repeat(50)
        );
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

    #[test]
    fn preset_levels() {
        assert_eq!(Preset::Fast.level(), 3);
        assert_eq!(Preset::Balanced.level(), 12);
        assert_eq!(Preset::Max.level(), 19);
        assert_eq!(Preset::parse("FAST").unwrap(), Preset::Fast);
        assert!(Preset::parse("slow").is_err());
    }

    #[test]
    fn stdin_style_round_trip() -> Result<()> {
        let data = b"stream me through memory";
        let mut compressed = Vec::new();
        compress_reader(Cursor::new(data), &mut compressed, 3, 1, None)?;
        let mut out = Vec::new();
        decompress_reader(Cursor::new(&compressed), &mut out, None)?;
        assert_eq!(out, data);
        Ok(())
    }

    #[test]
    fn size_mismatch_fails() -> Result<()> {
        let text = b"short";
        let mut container = Vec::new();
        write_header(
            &mut container,
            &Header {
                version: VERSION_V1,
                level: 3,
                original_len: 999, // wrong
                checksum: None,
            },
        )?;
        let mut encoder = zstd::stream::Encoder::new(&mut container, 3)?;
        encoder.write_all(text)?;
        encoder.finish()?;

        let err = decompress_reader(Cursor::new(&container), io::sink(), None)
            .expect_err("size mismatch should fail");
        assert!(err.to_string().contains("decompressed size mismatch"));
        Ok(())
    }
}
