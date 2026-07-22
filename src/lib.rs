//! Core library for the `rzc` file compressor.
//!
//! Provides the `.rzst` container format (RZC1), zstd compress/decompress,
//! integrity checking (SHA-256), multi-file pack archives (format v3),
//! and helpers for CLI tooling.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

/// Container magic bytes. Kept as `RZC1` across format versions.
pub const MAGIC: &[u8; 4] = b"RZC1";

/// Single-file container version (includes SHA-256 of original payload).
pub const VERSION: u8 = 2;

/// Legacy single-file container version without a checksum.
pub const VERSION_V1: u8 = 1;

/// Multi-file pack archive version.
pub const VERSION_V3: u8 = 3;

/// Length of the SHA-256 digest stored in v2/v3.
pub const HASH_LEN: usize = 32;

/// Default zstd level for the balanced preset / CLI default.
pub const DEFAULT_LEVEL: i32 = 12;

/// Default max decompressed member size for `grep` (32 MiB).
pub const DEFAULT_GREP_MAX_SIZE: u64 = 32 * 1024 * 1024;

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

/// Parsed single-file `.rzst` header (v1/v2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Header {
    /// Container version (`1` or `2`).
    pub version: u8,
    /// zstd compression level used at compress time.
    pub level: i32,
    /// Original uncompressed payload size in bytes.
    pub original_len: u64,
    /// SHA-256 of the original payload when `version >= 2`.
    #[serde(skip_serializing_if = "Option::is_none")]
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

/// Summary of a single-file compressed archive (for `info` and programmatic inspection).
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveInfo {
    pub header: Header,
    pub compressed_size: u64,
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    /// Always `"single"` for v1/v2 containers.
    pub kind: &'static str,
}

impl ArchiveInfo {
    /// Compression ratio as a percentage of original size.
    pub fn ratio_percent(&self) -> f64 {
        ratio_percent(self.compressed_size, self.header.original_len)
    }
}

/// One member of a v3 pack archive (metadata only).
#[derive(Debug, Clone, Serialize)]
pub struct PackMemberInfo {
    pub path: String,
    pub original_len: u64,
    pub compressed_len: u64,
    #[serde(serialize_with = "serialize_hash")]
    pub checksum: [u8; HASH_LEN],
}

impl PackMemberInfo {
    pub fn ratio_percent(&self) -> f64 {
        ratio_percent(self.compressed_len, self.original_len)
    }
}

/// Summary of a v3 multi-file pack archive.
#[derive(Debug, Clone, Serialize)]
pub struct PackInfo {
    pub version: u8,
    pub level: i32,
    pub file_count: u32,
    pub members: Vec<PackMemberInfo>,
    pub archive_size: u64,
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    /// Always `"pack"` for v3 archives.
    pub kind: &'static str,
}

impl PackInfo {
    pub fn total_original_len(&self) -> u64 {
        self.members.iter().map(|m| m.original_len).sum()
    }

    pub fn total_compressed_payload(&self) -> u64 {
        self.members.iter().map(|m| m.compressed_len).sum()
    }

    pub fn ratio_percent(&self) -> f64 {
        ratio_percent(self.archive_size, self.total_original_len())
    }
}

/// Unified listing result for `list` / `info`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ListResult {
    Single(ArchiveInfo),
    Pack(PackInfo),
}

/// Result statistics for a compress/decompress operation.
#[derive(Debug, Clone, Serialize)]
pub struct IoStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    #[serde(serialize_with = "serialize_path")]
    pub output_path: PathBuf,
}

/// Dry-run estimate of compressed size (no files written).
#[derive(Debug, Clone, Serialize)]
pub struct DryRunStats {
    pub input_bytes: u64,
    pub estimated_compressed_bytes: u64,
    pub ratio_percent: f64,
    #[serde(serialize_with = "serialize_path")]
    pub input_path: PathBuf,
}

/// Result of a pack operation.
#[derive(Debug, Clone, Serialize)]
pub struct PackStats {
    pub file_count: u32,
    pub original_bytes: u64,
    pub archive_bytes: u64,
    #[serde(serialize_with = "serialize_path")]
    pub output_path: PathBuf,
}

/// Result of unpacking one member.
#[derive(Debug, Clone, Serialize)]
pub struct UnpackMemberStats {
    pub path: String,
    pub original_bytes: u64,
    pub skipped: bool,
}

/// Aggregate unpack result.
#[derive(Debug, Clone, Serialize)]
pub struct UnpackStats {
    pub members: Vec<UnpackMemberStats>,
    pub written: u32,
    pub skipped: u32,
    #[serde(serialize_with = "serialize_path")]
    pub output_dir: PathBuf,
}

/// Options for unpacking a v3 pack archive.
#[derive(Debug, Clone, Default)]
pub struct UnpackOpts {
    /// Skip members whose destination already exists.
    pub skip_existing: bool,
    /// Only extract the member with this exact archive path (if set).
    pub only: Option<String>,
    /// Strip this many leading path components (like `tar --strip-components`).
    pub strip_components: u32,
    /// Overwrite existing destination files. When false, existing files error
    /// unless [`Self::skip_existing`] is true.
    pub force: bool,
}

/// Options for packing a directory into a v3 archive.
#[derive(Debug, Clone, Default)]
pub struct PackOpts {
    /// Glob patterns to exclude.
    pub excludes: Vec<String>,
    /// Only include files modified within the last N days.
    pub newer_than_days: Option<u64>,
    /// Overwrite the output archive if it already exists.
    pub force: bool,
}

/// One side of an archive diff entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// Present only in the first archive.
    OnlyInA,
    /// Present only in the second archive.
    OnlyInB,
    /// Same path in both, different checksum / content.
    Changed,
    /// Same path and same checksum.
    Identical,
}

/// A single path comparison between two archives.
#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    pub path: String,
    pub status: DiffStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_a: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_b: Option<String>,
}

/// Result of comparing two archives.
#[derive(Debug, Clone, Serialize)]
pub struct DiffResult {
    #[serde(serialize_with = "serialize_path")]
    pub archive_a: PathBuf,
    #[serde(serialize_with = "serialize_path")]
    pub archive_b: PathBuf,
    pub entries: Vec<DiffEntry>,
    pub only_in_a: u32,
    pub only_in_b: u32,
    pub changed: u32,
    pub identical: u32,
}

impl DiffResult {
    /// True when both archives have the same members and checksums.
    pub fn is_equal(&self) -> bool {
        self.only_in_a == 0 && self.only_in_b == 0 && self.changed == 0
    }
}

/// Doctor self-test result.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub zstd_roundtrip: bool,
    pub container_v2_roundtrip: bool,
    pub pack_v3_roundtrip: bool,
    pub messages: Vec<String>,
}

/// One match from grepping archive member contents.
#[derive(Debug, Clone, Serialize)]
pub struct GrepMatch {
    pub member: String,
    pub line_number: usize,
    pub line: String,
}

/// Result of grepping an archive.
#[derive(Debug, Clone, Serialize)]
pub struct GrepResult {
    pub pattern: String,
    pub matches: Vec<GrepMatch>,
    pub members_searched: u32,
    pub members_skipped: u32,
    pub match_count: u32,
}

/// Result of writing or verifying a SHA-256 integrity sidecar.
#[derive(Debug, Clone, Serialize)]
pub struct SealInfo {
    #[serde(serialize_with = "serialize_path")]
    pub archive: PathBuf,
    #[serde(serialize_with = "serialize_path")]
    pub sidecar: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Result of repacking a pack archive with member filters.
#[derive(Debug, Clone, Serialize)]
pub struct RepackStats {
    pub kept: u32,
    pub excluded: u32,
    pub original_bytes: u64,
    pub archive_bytes: u64,
    #[serde(serialize_with = "serialize_path")]
    pub output_path: PathBuf,
}

/// Optional progress callback: `(bytes_processed, total_hint)`.
/// `total_hint` may be 0 when unknown.
pub type ProgressFn<'a> = dyn Fn(u64, u64) + 'a;

fn serialize_path<S>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&path.display().to_string())
}

fn serialize_hash<S>(hash: &[u8; HASH_LEN], serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&hex::encode(hash))
}

// ---------------------------------------------------------------------------
// Header I/O (v1 / v2 single-file)
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
    } else if header.version >= VERSION && header.version != VERSION_V3 {
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

/// Read and validate an RZC1 single-file header (v1 or v2).
///
/// Returns an error if the stream is a v3 pack archive — use [`peek_version`]
/// or [`list_archive`] for pack files.
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
    if version == VERSION_V3 {
        bail!("this is a multi-file pack archive (v3); use list/unpack instead of single-file decompress");
    }
    if version != VERSION_V1 && version != VERSION {
        bail!(
            "unsupported container version {version}; supported versions are {VERSION_V1}, {VERSION}, and {VERSION_V3}"
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

/// Peek the container version without consuming more of the file than needed.
pub fn peek_version(path: &Path) -> Result<u8> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .context("reading compressor magic")?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }
    let mut version = [0_u8; 1];
    file.read_exact(&mut version)
        .context("reading container version")?;
    Ok(version[0])
}

// ---------------------------------------------------------------------------
// Compress / decompress (streaming over Read/Write) — single-file v2
// ---------------------------------------------------------------------------

/// Compress `reader` into `writer` using the v2 container format.
///
/// The full input is hashed with SHA-256 while being compressed. Because the
/// header must record `original_len` and the checksum up front, the input is
/// fully read into memory first.
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

/// Compress payload bytes with zstd only (no RZC header). Used by pack members.
fn zstd_compress_raw(data: &[u8], level: i32, threads: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = zstd::stream::Encoder::new(&mut out, level)
        .with_context(|| format!("creating zstd encoder at level {level}"))?;
    if threads > 1 {
        encoder
            .multithread(threads)
            .with_context(|| format!("enabling {threads} zstd worker threads"))?;
    }
    encoder.write_all(data).context("writing to zstd encoder")?;
    encoder.finish().context("finishing zstd frame")?;
    Ok(out)
}

/// Read a blob whose length was declared by an archive header.
///
/// The buffer grows from what the archive actually contains instead of being
/// pre-allocated at the declared size. A hostile header can claim any length,
/// and `vec![0; declared]` would ask the allocator for it outright — a 69-byte
/// crafted archive declaring 281 TB aborted the process before any validation
/// ran. Reading through `take` bounds memory by the real remaining bytes, so a
/// bogus length becomes an ordinary error.
fn read_declared(reader: &mut impl Read, len: u64, what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let read = reader
        .take(len)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {what}"))?;
    if read as u64 != len {
        bail!("{what}: header declares {len} bytes but only {read} remain in the archive");
    }
    Ok(buf)
}

/// Decompress a raw zstd frame into memory.
fn zstd_decompress_raw(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder =
        zstd::stream::Decoder::new(Cursor::new(data)).context("creating zstd decoder")?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .context("decompressing zstd frame")?;
    Ok(out)
}

/// Write that discards data but counts bytes written (for dry-run).
#[derive(Default)]
pub struct CountingSink {
    pub bytes: u64,
}

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Estimate compressed size without writing an output file.
pub fn compress_bytes_dry_run(data: &[u8], level: i32, threads: u32) -> Result<DryRunStats> {
    let mut sink = CountingSink::default();
    compress_bytes(data, &mut sink, level, threads, None)?;
    let input_bytes = data.len() as u64;
    Ok(DryRunStats {
        input_bytes,
        estimated_compressed_bytes: sink.bytes,
        ratio_percent: ratio_percent(sink.bytes, input_bytes),
        input_path: PathBuf::from("<memory>"),
    })
}

/// Dry-run compress a filesystem path (no output written).
pub fn compress_file_dry_run(input: &Path, level: i32, threads: u32) -> Result<DryRunStats> {
    let data = fs::read(input).with_context(|| format!("reading input {}", input.display()))?;
    let mut stats = compress_bytes_dry_run(&data, level, threads)?;
    stats.input_path = input.to_path_buf();
    Ok(stats)
}

/// Compress a filesystem path to another path.
///
/// Overwrites `output` if it already exists. Prefer [`compress_file_opts`] to
/// control overwrite behaviour.
pub fn compress_file(
    input: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    compress_file_opts(input, output, level, threads, true, progress)
}

/// Compress a filesystem path with overwrite control.
///
/// When `force` is false and `output` already exists, returns an error.
pub fn compress_file_opts(
    input: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    force: bool,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    if output.exists() && !force {
        bail!(
            "output already exists: {} (use --force to overwrite)",
            output.display()
        );
    }

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
    decompress_file_opts(input, output, false, progress)
}

/// Decompress with options (e.g. skip existing).
pub fn decompress_file_opts(
    input: &Path,
    output: &Path,
    skip_existing: bool,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IoStats> {
    if skip_existing && output.exists() {
        let input_bytes = fs::metadata(input)
            .with_context(|| format!("reading metadata for {}", input.display()))?
            .len();
        let output_bytes = fs::metadata(output)
            .with_context(|| format!("reading metadata for {}", output.display()))?
            .len();
        return Ok(IoStats {
            input_bytes,
            output_bytes,
            output_path: output.to_path_buf(),
        });
    }

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
    let output_bytes = decompress_reader(reader, writer, progress)?;
    Ok(IoStats {
        input_bytes: 0,
        output_bytes,
        output_path: output.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// Info / verify / list
// ---------------------------------------------------------------------------

/// Parse single-file archive metadata without fully decompressing.
pub fn inspect_file(path: &Path) -> Result<ArchiveInfo> {
    let version = peek_version(path)?;
    if version == VERSION_V3 {
        bail!(
            "{} is a multi-file pack archive (v3); use `rzc list` or `rzc info`",
            path.display()
        );
    }

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
        kind: "single",
    })
}

/// List contents of a single-file or pack archive.
pub fn list_archive(path: &Path) -> Result<ListResult> {
    let version = peek_version(path)?;
    if version == VERSION_V3 {
        Ok(ListResult::Pack(inspect_pack(path)?))
    } else {
        Ok(ListResult::Single(inspect_file(path)?))
    }
}

/// Inspect a v3 pack archive (metadata only; streams member headers).
pub fn inspect_pack(path: &Path) -> Result<PackInfo> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let archive_size = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }
    let mut version = [0_u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != VERSION_V3 {
        bail!(
            "expected pack archive version {VERSION_V3}, got {}",
            version[0]
        );
    }
    let mut level_buf = [0_u8; 4];
    reader.read_exact(&mut level_buf)?;
    let level = i32::from_le_bytes(level_buf);
    let mut count_buf = [0_u8; 4];
    reader.read_exact(&mut count_buf)?;
    let file_count = u32::from_le_bytes(count_buf);

    let mut members = Vec::with_capacity((file_count as usize).min(1024));
    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        if path_len > 16 * 1024 {
            bail!("pack member path too long ({path_len} bytes)");
        }
        let path_bytes = read_declared(&mut reader, path_len as u64, "pack member path")?;
        let member_path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        // Skip compressed payload. `take` stops at EOF without complaining, so
        // compare what was actually skipped: otherwise a member declaring more
        // bytes than the archive holds is reported as valid metadata.
        let skipped = io::copy(&mut reader.by_ref().take(compressed_len), &mut io::sink())
            .context("skipping compressed member payload")?;
        if skipped != compressed_len {
            bail!(
                "pack member '{member_path}' declares {compressed_len} compressed bytes \
                 but only {skipped} remain in the archive"
            );
        }

        members.push(PackMemberInfo {
            path: member_path,
            original_len,
            compressed_len,
            checksum,
        });
    }

    Ok(PackInfo {
        version: VERSION_V3,
        level,
        file_count,
        members,
        archive_size,
        path: path.to_path_buf(),
        kind: "pack",
    })
}

/// Decompress to a sink, verifying original size and checksum (single-file only).
pub fn verify_file(path: &Path, progress: Option<&ProgressFn<'_>>) -> Result<u64> {
    let version = peek_version(path)?;
    if version == VERSION_V3 {
        // Verify every pack member.
        return verify_pack(path, progress);
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    decompress_reader(reader, io::sink(), progress)
}

/// Verify all members of a pack archive.
pub fn verify_pack(path: &Path, progress: Option<&ProgressFn<'_>>) -> Result<u64> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid file magic");
    }
    let mut version = [0_u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != VERSION_V3 {
        bail!("not a v3 pack archive");
    }
    let mut level_buf = [0_u8; 4];
    reader.read_exact(&mut level_buf)?;
    let mut count_buf = [0_u8; 4];
    reader.read_exact(&mut count_buf)?;
    let file_count = u32::from_le_bytes(count_buf);

    let mut total: u64 = 0;
    for i in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        // This pass only totals sizes; the path itself is skipped over.
        read_declared(&mut reader, path_len as u64, "pack member path")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let compressed = read_declared(
            &mut reader,
            compressed_len,
            &format!("pack member {i} payload"),
        )?;

        let plain = zstd_decompress_raw(&compressed)?;
        if plain.len() as u64 != original_len {
            bail!(
                "pack member size mismatch for entry {i}: expected {original_len}, got {}",
                plain.len()
            );
        }
        let actual = sha256(&plain);
        if actual != checksum {
            bail!(
                "pack member checksum mismatch for entry {i}: expected {}, got {}",
                hex::encode(checksum),
                hex::encode(actual)
            );
        }
        total += original_len;
        if let Some(cb) = progress {
            cb(total, 0);
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Pack / unpack (format v3)
// ---------------------------------------------------------------------------

/// Pack a directory into a v3 multi-file archive.
///
/// Layout:
/// ```text
/// magic(4) version=3(1) level(i32) file_count(u32)
/// for each file:
///   path_len(u32) path(utf8) original_len(u64) sha256(32)
///   compressed_len(u64) compressed_bytes
/// ```
///
/// Overwrites `output` if it exists. Prefer [`pack_directory_opts`] for filters
/// and overwrite control.
pub fn pack_directory(
    dir: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    excludes: &[String],
) -> Result<PackStats> {
    pack_directory_opts(
        dir,
        output,
        level,
        threads,
        &PackOpts {
            excludes: excludes.to_vec(),
            newer_than_days: None,
            force: true,
        },
        None,
    )
}

/// Pack a directory with filters, force, and optional progress.
///
/// Progress callback receives `(files_done, files_total)` — both are file
/// counts (not bytes) so many small files show a solid overall bar.
pub fn pack_directory_opts(
    dir: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    opts: &PackOpts,
    progress: Option<&ProgressFn<'_>>,
) -> Result<PackStats> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    if output.exists() && !opts.force {
        bail!(
            "output already exists: {} (use --force to overwrite)",
            output.display()
        );
    }

    let exclude_set = build_exclude_set(&opts.excludes)?;
    let mtime_cutoff = opts.newer_than_days.map(|days| {
        SystemTime::now()
            .checked_sub(Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });

    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(dir)
            .with_context(|| format!("path {} not under {}", abs.display(), dir.display()))?;
        let rel_str = normalize_archive_path(rel)?;
        if is_excluded(&rel_str, &exclude_set) {
            continue;
        }
        if let Some(cutoff) = mtime_cutoff {
            let modified = fs::metadata(abs)
                .and_then(|m| m.modified())
                .with_context(|| format!("reading mtime for {}", abs.display()))?;
            if modified < cutoff {
                continue;
            }
        }
        files.push((rel_str, abs.to_path_buf()));
    }

    if files.is_empty() {
        bail!("no files to pack under {}", dir.display());
    }
    if files.len() > u32::MAX as usize {
        bail!("too many files to pack");
    }

    let total_files = files.len() as u64;
    if let Some(cb) = progress {
        cb(0, total_files);
    }

    // Compress independent members in parallel (rayon); write in original order.
    // Progress is reported when compression finishes (callback is not Sync).
    let packed: Vec<(String, u64, [u8; HASH_LEN], Vec<u8>)> = files
        .par_iter()
        .map(
            |(rel_path, abs_path)| -> Result<(String, u64, [u8; HASH_LEN], Vec<u8>)> {
                let data = fs::read(abs_path)
                    .with_context(|| format!("reading {}", abs_path.display()))?;
                let checksum = sha256(&data);
                let compressed = zstd_compress_raw(&data, level, threads)?;
                Ok((rel_path.clone(), data.len() as u64, checksum, compressed))
            },
        )
        .collect::<Result<Vec<_>>>()?;

    if let Some(cb) = progress {
        cb(total_files, total_files);
    }

    ensure_parent_dir(output)?;
    let out_file =
        File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut writer = BufWriter::new(out_file);

    writer.write_all(MAGIC)?;
    writer.write_all(&[VERSION_V3])?;
    writer.write_all(&level.to_le_bytes())?;
    writer.write_all(&(packed.len() as u32).to_le_bytes())?;

    let mut original_bytes: u64 = 0;
    for (rel_path, original_len, checksum, compressed) in &packed {
        let path_bytes = rel_path.as_bytes();
        if path_bytes.len() > u32::MAX as usize {
            bail!("path too long: {rel_path}");
        }

        writer.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
        writer.write_all(path_bytes)?;
        writer.write_all(&original_len.to_le_bytes())?;
        writer.write_all(checksum)?;
        writer.write_all(&(compressed.len() as u64).to_le_bytes())?;
        writer.write_all(compressed)?;

        original_bytes += original_len;
    }

    writer.flush()?;
    drop(writer);

    let archive_bytes = fs::metadata(output)
        .with_context(|| format!("reading metadata for {}", output.display()))?
        .len();

    Ok(PackStats {
        file_count: packed.len() as u32,
        original_bytes,
        archive_bytes,
        output_path: output.to_path_buf(),
    })
}

/// Unpack a v3 archive into `output_dir` (overwrites existing members).
pub fn unpack_archive(
    archive: &Path,
    output_dir: &Path,
    skip_existing: bool,
) -> Result<UnpackStats> {
    unpack_archive_opts(
        archive,
        output_dir,
        &UnpackOpts {
            skip_existing,
            only: None,
            strip_components: 0,
            force: true,
        },
    )
}

/// Unpack with selective extract, strip-components, and force control.
pub fn unpack_archive_opts(
    archive: &Path,
    output_dir: &Path,
    opts: &UnpackOpts,
) -> Result<UnpackStats> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }
    let mut version = [0_u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != VERSION_V3 {
        bail!(
            "expected pack archive version {VERSION_V3}, got {}; use `rzc decompress` for single-file archives",
            version[0]
        );
    }
    let mut level_buf = [0_u8; 4];
    reader.read_exact(&mut level_buf)?;
    let mut count_buf = [0_u8; 4];
    reader.read_exact(&mut count_buf)?;
    let file_count = u32::from_le_bytes(count_buf);

    fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output directory {}", output_dir.display()))?;

    let mut members = Vec::with_capacity((file_count as usize).min(1024));
    let mut written = 0_u32;
    let mut skipped = 0_u32;
    let mut matched_only = false;

    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        let path_bytes = read_declared(&mut reader, path_len as u64, "pack member path")?;
        let member_path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let compressed = read_declared(
            &mut reader,
            compressed_len,
            &format!("compressed data for {member_path}"),
        )?;

        if let Some(only) = &opts.only {
            if member_path != *only {
                continue;
            }
            matched_only = true;
        }

        let Some(stripped) = strip_path_components(&member_path, opts.strip_components) else {
            // Entire path stripped — skip this member.
            members.push(UnpackMemberStats {
                path: member_path,
                original_bytes: original_len,
                skipped: true,
            });
            skipped += 1;
            continue;
        };

        let dest = safe_join(output_dir, &stripped)?;
        if dest.exists() {
            if opts.skip_existing {
                members.push(UnpackMemberStats {
                    path: member_path,
                    original_bytes: original_len,
                    skipped: true,
                });
                skipped += 1;
                continue;
            }
            if !opts.force {
                bail!(
                    "output already exists: {} (use --force to overwrite or --skip-existing)",
                    dest.display()
                );
            }
        }

        let plain = zstd_decompress_raw(&compressed)
            .with_context(|| format!("decompressing {member_path}"))?;
        if plain.len() as u64 != original_len {
            bail!(
                "size mismatch for {member_path}: expected {original_len}, got {}",
                plain.len()
            );
        }
        let actual = sha256(&plain);
        if actual != checksum {
            bail!(
                "checksum mismatch for {member_path}: expected {}, got {}",
                hex::encode(checksum),
                hex::encode(actual)
            );
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&dest, &plain).with_context(|| format!("writing {}", dest.display()))?;

        members.push(UnpackMemberStats {
            path: member_path,
            original_bytes: original_len,
            skipped: false,
        });
        written += 1;
    }

    if let Some(only) = &opts.only {
        if !matched_only {
            bail!("member not found in archive: {only}");
        }
    }

    Ok(UnpackStats {
        members,
        written,
        skipped,
        output_dir: output_dir.to_path_buf(),
    })
}

/// Decompress a single pack member (or whole single-file archive) to `writer`.
///
/// For v3 packs, `member` must be the archive-relative path. For v1/v2
/// single-file archives, `member` is ignored.
pub fn cat_member(archive: &Path, member: Option<&str>, mut writer: impl Write) -> Result<u64> {
    let version = peek_version(archive)?;
    if version == VERSION_V3 {
        let member =
            member.ok_or_else(|| anyhow::anyhow!("member path is required for pack archives"))?;
        let data = extract_pack_member(archive, member)?;
        writer
            .write_all(&data)
            .context("writing member data to output")?;
        writer.flush().context("flushing output")?;
        return Ok(data.len() as u64);
    }

    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let written = decompress_reader(BufReader::new(file), writer, None)?;
    Ok(written)
}

/// Extract and verify one pack member into memory.
pub fn extract_pack_member(archive: &Path, member_path: &str) -> Result<Vec<u8>> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }
    let mut version = [0_u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != VERSION_V3 {
        bail!(
            "expected pack archive version {VERSION_V3}, got {}",
            version[0]
        );
    }
    let mut level_buf = [0_u8; 4];
    reader.read_exact(&mut level_buf)?;
    let mut count_buf = [0_u8; 4];
    reader.read_exact(&mut count_buf)?;
    let file_count = u32::from_le_bytes(count_buf);

    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        let path_bytes = read_declared(&mut reader, path_len as u64, "pack member path")?;
        let path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let compressed = read_declared(
            &mut reader,
            compressed_len,
            &format!("compressed data for {path}"),
        )?;

        if path != member_path {
            continue;
        }

        let plain =
            zstd_decompress_raw(&compressed).with_context(|| format!("decompressing {path}"))?;
        if plain.len() as u64 != original_len {
            bail!(
                "size mismatch for {path}: expected {original_len}, got {}",
                plain.len()
            );
        }
        let actual = sha256(&plain);
        if actual != checksum {
            bail!(
                "checksum mismatch for {path}: expected {}, got {}",
                hex::encode(checksum),
                hex::encode(actual)
            );
        }
        return Ok(plain);
    }

    bail!("member not found in archive: {member_path}");
}

/// Compare two archives by member path and checksum (v3) or single-file hash.
pub fn diff_archives(a: &Path, b: &Path) -> Result<DiffResult> {
    let map_a = archive_checksum_map(a)?;
    let map_b = archive_checksum_map(b)?;

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(map_a.keys().cloned());
    paths.extend(map_b.keys().cloned());

    let mut entries = Vec::new();
    let mut only_in_a = 0_u32;
    let mut only_in_b = 0_u32;
    let mut changed = 0_u32;
    let mut identical = 0_u32;

    for path in paths {
        let ca = map_a.get(&path);
        let cb = map_b.get(&path);
        let (status, checksum_a, checksum_b) = match (ca, cb) {
            (Some(ha), Some(hb)) if ha == hb => {
                identical += 1;
                (
                    DiffStatus::Identical,
                    Some(hex::encode(ha)),
                    Some(hex::encode(hb)),
                )
            }
            (Some(ha), Some(hb)) => {
                changed += 1;
                (
                    DiffStatus::Changed,
                    Some(hex::encode(ha)),
                    Some(hex::encode(hb)),
                )
            }
            (Some(ha), None) => {
                only_in_a += 1;
                (DiffStatus::OnlyInA, Some(hex::encode(ha)), None)
            }
            (None, Some(hb)) => {
                only_in_b += 1;
                (DiffStatus::OnlyInB, None, Some(hex::encode(hb)))
            }
            (None, None) => unreachable!(),
        };
        entries.push(DiffEntry {
            path,
            status,
            checksum_a,
            checksum_b,
        });
    }

    Ok(DiffResult {
        archive_a: a.to_path_buf(),
        archive_b: b.to_path_buf(),
        entries,
        only_in_a,
        only_in_b,
        changed,
        identical,
    })
}

/// Build a path → checksum map for single-file or pack archives.
fn archive_checksum_map(path: &Path) -> Result<BTreeMap<String, [u8; HASH_LEN]>> {
    let version = peek_version(path)?;
    if version == VERSION_V3 {
        let info = inspect_pack(path)?;
        let mut map = BTreeMap::new();
        for m in info.members {
            map.insert(m.path, m.checksum);
        }
        return Ok(map);
    }

    let info = inspect_file(path)?;
    let checksum = match info.header.checksum {
        Some(c) => c,
        None => {
            // v1: decompress and hash.
            let mut out = Vec::new();
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            decompress_reader(BufReader::new(file), &mut out, None)?;
            sha256(&out)
        }
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<single>".into());
    let mut map = BTreeMap::new();
    map.insert(name, checksum);
    Ok(map)
}

/// Strip the first `n` components from an archive path. Returns `None` if the
/// path becomes empty (entirely stripped).
pub fn strip_path_components(path: &str, n: u32) -> Option<String> {
    if n == 0 {
        return Some(path.to_string());
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if (n as usize) >= parts.len() {
        return None;
    }
    Some(parts[n as usize..].join("/"))
}

/// Normalize a relative path for storage in the archive (forward slashes, no `..`).
fn normalize_archive_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if s.contains('/') || s.contains('\\') {
                    bail!("invalid path component in {}", path.display());
                }
                parts.push(s.into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("path must not contain '..': {}", path.display()),
            Component::RootDir | Component::Prefix(_) => {
                bail!("path must be relative: {}", path.display())
            }
        }
    }
    if parts.is_empty() {
        bail!("empty relative path");
    }
    Ok(parts.join("/"))
}

/// Join `base` with an archive-relative path, rejecting traversal.
fn safe_join(base: &Path, archive_path: &str) -> Result<PathBuf> {
    if archive_path.is_empty() || archive_path.starts_with('/') || archive_path.starts_with('\\') {
        bail!("invalid archive path: {archive_path}");
    }
    let mut out = base.to_path_buf();
    for part in archive_path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            bail!("path traversal rejected: {archive_path}");
        }
        if part.contains('\\') {
            bail!("invalid path component: {part}");
        }
        out.push(part);
    }
    // Extra guard: resolved path must stay under base.
    let base_canon = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    // dest may not exist yet; check parent chain intent via components.
    let rel = out.strip_prefix(base).unwrap_or(&out);
    for c in rel.components() {
        if matches!(c, Component::ParentDir) {
            bail!("path traversal rejected: {archive_path}");
        }
    }
    let _ = base_canon; // kept for future strict canonicalize checks
    Ok(out)
}

// ---------------------------------------------------------------------------
// Recursive directory helpers + exclude globs
// ---------------------------------------------------------------------------

/// Build a [`GlobSet`] from exclude patterns. Patterns match against the relative
/// path using forward slashes, and also against the final path component.
pub fn build_exclude_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p).with_context(|| format!("invalid exclude glob '{p}'"))?;
        builder.add(glob);
        // Also allow matching directory names without wildcards via **/name/**
        if !p.contains('*') && !p.contains('?') && !p.contains('[') {
            let as_dir = format!("**/{p}/**");
            if let Ok(g) = Glob::new(&as_dir) {
                builder.add(g);
            }
            let as_file = format!("**/{p}");
            if let Ok(g) = Glob::new(&as_file) {
                builder.add(g);
            }
        }
    }
    Ok(Some(builder.build().context("building exclude glob set")?))
}

/// Whether a relative path (forward slashes) is excluded.
pub fn is_excluded(rel_path: &str, set: &Option<GlobSet>) -> bool {
    let Some(set) = set else {
        return false;
    };
    if set.is_match(rel_path) {
        return true;
    }
    // Match final component alone (e.g. pattern `target` vs `src/target`).
    if let Some(name) = rel_path.rsplit('/').next() {
        if set.is_match(name) {
            return true;
        }
    }
    false
}

/// Compress every regular file under `dir` into a sibling `.rzst` next to it.
/// Skips files that already end with `.rzst`.
pub fn compress_dir_recursive(
    dir: &Path,
    level: i32,
    threads: u32,
    progress: Option<&ProgressFn<'_>>,
) -> Result<Vec<IoStats>> {
    compress_dir_recursive_ex(dir, level, threads, &[], false, progress)
}

/// Recursive compress with exclude globs and optional dry-run.
///
/// When `dry_run` is true, no files are written; returned `IoStats.output_bytes`
/// is the estimated compressed size and `output_path` is the would-be path.
pub fn compress_dir_recursive_ex(
    dir: &Path,
    level: i32,
    threads: u32,
    excludes: &[String],
    dry_run: bool,
    progress: Option<&ProgressFn<'_>>,
) -> Result<Vec<IoStats>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let exclude_set = build_exclude_set(excludes)?;
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

        let rel = path
            .strip_prefix(dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if is_excluded(&rel, &exclude_set) {
            continue;
        }

        let output = append_suffix(path, ".rzst");
        if dry_run {
            let dry = compress_file_dry_run(path, level, threads)?;
            results.push(IoStats {
                input_bytes: dry.input_bytes,
                output_bytes: dry.estimated_compressed_bytes,
                output_path: output,
            });
        } else {
            let stats = compress_file(path, &output, level, threads, progress)?;
            results.push(stats);
        }
    }
    Ok(results)
}

/// Decompress every `.rzst` file under `dir` to the default output path.
pub fn decompress_dir_recursive(
    dir: &Path,
    progress: Option<&ProgressFn<'_>>,
) -> Result<Vec<IoStats>> {
    decompress_dir_recursive_ex(dir, false, progress)
}

/// Recursive decompress with skip-existing option.
pub fn decompress_dir_recursive_ex(
    dir: &Path,
    skip_existing: bool,
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
        // Skip v3 pack archives in recursive single-file mode.
        if peek_version(path).ok() == Some(VERSION_V3) {
            continue;
        }
        let output = default_decompressed_path(path);
        let stats = decompress_file_opts(path, &output, skip_existing, progress)?;
        results.push(stats);
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Doctor (self-test)
// ---------------------------------------------------------------------------

/// Run in-memory round-trip checks to confirm zstd + container formats work.
pub fn doctor() -> DoctorReport {
    let mut messages = Vec::new();
    let mut ok = true;

    // 1. Raw zstd roundtrip
    let zstd_roundtrip = match (|| -> Result<()> {
        let data = b"doctor zstd probe data with some repetition repetition";
        let compressed = zstd_compress_raw(data, 3, 1)?;
        let plain = zstd_decompress_raw(&compressed)?;
        if plain != data {
            bail!("zstd payload mismatch");
        }
        Ok(())
    })() {
        Ok(()) => {
            messages.push("zstd roundtrip: ok".into());
            true
        }
        Err(e) => {
            messages.push(format!("zstd roundtrip: FAIL ({e:#})"));
            ok = false;
            false
        }
    };

    // 2. v2 container
    let container_v2_roundtrip = match (|| -> Result<()> {
        let data = b"container v2 integrity bytes ".repeat(32);
        let mut buf = Vec::new();
        compress_bytes(&data, &mut buf, 3, 1, None)?;
        let mut out = Vec::new();
        decompress_reader(Cursor::new(&buf), &mut out, None)?;
        if out != data {
            bail!("v2 payload mismatch");
        }
        Ok(())
    })() {
        Ok(()) => {
            messages.push("container v2 roundtrip: ok".into());
            true
        }
        Err(e) => {
            messages.push(format!("container v2 roundtrip: FAIL ({e:#})"));
            ok = false;
            false
        }
    };

    // 3. Pack v3 in temp dir
    let pack_v3_roundtrip = match (|| -> Result<()> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("rzc-doctor-{}-{stamp}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let out_dir = root.join("out");
        fs::create_dir_all(src.join("sub"))?;
        fs::write(src.join("a.txt"), b"alpha alpha alpha")?;
        fs::write(src.join("sub/b.txt"), b"beta beta beta")?;
        let archive = root.join("bundle.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;
        unpack_archive(&archive, &out_dir, false)?;
        if fs::read(out_dir.join("a.txt"))? != b"alpha alpha alpha" {
            bail!("pack member a.txt mismatch");
        }
        if fs::read(out_dir.join("sub/b.txt"))? != b"beta beta beta" {
            bail!("pack member sub/b.txt mismatch");
        }
        let _ = fs::remove_dir_all(&root);
        Ok(())
    })() {
        Ok(()) => {
            messages.push("pack v3 roundtrip: ok".into());
            true
        }
        Err(e) => {
            messages.push(format!("pack v3 roundtrip: FAIL ({e:#})"));
            ok = false;
            false
        }
    };

    if ok {
        messages.push("all checks passed".into());
    }

    DoctorReport {
        ok,
        zstd_roundtrip,
        container_v2_roundtrip,
        pack_v3_roundtrip,
        messages,
    }
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

// ---------------------------------------------------------------------------
// Tree / grep / seal / repack
// ---------------------------------------------------------------------------

/// Build a tree string of member paths (for v3 packs; also works for a path list).
pub fn format_path_tree(paths: &[String]) -> String {
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        is_file: bool,
    }

    let mut root = Node::default();
    for path in paths {
        let mut cur = &mut root;
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            let is_last = i + 1 == parts.len();
            let child = cur.children.entry((*part).to_string()).or_default();
            if is_last {
                child.is_file = true;
            }
            cur = child;
        }
    }

    fn render(node: &Node, prefix: &str, lines: &mut Vec<String>) {
        let entries: Vec<_> = node.children.iter().collect();
        for (i, (name, child)) in entries.iter().enumerate() {
            let is_last = i + 1 == entries.len();
            let branch = if is_last { "└── " } else { "├── " };
            lines.push(format!("{prefix}{branch}{name}"));
            let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
            render(child, &child_prefix, lines);
        }
    }

    let mut lines = Vec::new();
    render(&root, "", &mut lines);
    lines.join("\n")
}

/// Print member paths of a pack archive as a directory tree.
pub fn archive_tree(path: &Path) -> Result<String> {
    let version = peek_version(path)?;
    if version != VERSION_V3 {
        bail!("tree requires a v3 pack archive (got version {version})");
    }
    let info = inspect_pack(path)?;
    let paths: Vec<String> = info.members.iter().map(|m| m.path.clone()).collect();
    let mut out = format!("{}\n", path.display());
    let tree = format_path_tree(&paths);
    if !tree.is_empty() {
        out.push_str(&tree);
        out.push('\n');
    }
    Ok(out)
}

/// Grep decompressed text of archive members (single-file or pack).
///
/// Members whose original size exceeds `max_size` are skipped.
/// Pattern is a Rust regex (substring matches when used as a literal).
pub fn grep_archive(path: &Path, pattern: &str, max_size: u64) -> Result<GrepResult> {
    let re = Regex::new(pattern).with_context(|| format!("invalid regex pattern: {pattern}"))?;
    let version = peek_version(path)?;

    let mut matches = Vec::new();
    let mut members_searched = 0_u32;
    let mut members_skipped = 0_u32;

    if version == VERSION_V3 {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = BufReader::new(file);

        let mut magic = [0_u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            bail!("invalid file magic");
        }
        let mut version_buf = [0_u8; 1];
        reader.read_exact(&mut version_buf)?;
        let mut level_buf = [0_u8; 4];
        reader.read_exact(&mut level_buf)?;
        let mut count_buf = [0_u8; 4];
        reader.read_exact(&mut count_buf)?;
        let file_count = u32::from_le_bytes(count_buf);

        for _ in 0..file_count {
            let mut path_len_buf = [0_u8; 4];
            reader.read_exact(&mut path_len_buf)?;
            let path_len = u32::from_le_bytes(path_len_buf) as usize;
            let path_bytes = read_declared(&mut reader, path_len as u64, "pack member path")?;
            let member_path =
                String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

            let mut original_len_buf = [0_u8; 8];
            reader.read_exact(&mut original_len_buf)?;
            let original_len = u64::from_le_bytes(original_len_buf);

            let mut checksum = [0_u8; HASH_LEN];
            reader.read_exact(&mut checksum)?;

            let mut compressed_len_buf = [0_u8; 8];
            reader.read_exact(&mut compressed_len_buf)?;
            let compressed_len = u64::from_le_bytes(compressed_len_buf);

            let compressed = read_declared(&mut reader, compressed_len, "pack member payload")?;

            if original_len > max_size {
                members_skipped += 1;
                continue;
            }

            let plain = zstd_decompress_raw(&compressed)?;
            if plain.len() as u64 != original_len {
                bail!(
                    "size mismatch in member {member_path}: expected {original_len}, got {}",
                    plain.len()
                );
            }
            let actual = sha256(&plain);
            if actual != checksum {
                bail!("checksum mismatch for member {member_path}");
            }

            members_searched += 1;
            grep_plain(&member_path, &plain, &re, &mut matches);
        }
    } else {
        let info = inspect_file(path)?;
        if info.header.original_len > max_size {
            members_skipped = 1;
        } else {
            let mut plain = Vec::new();
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            decompress_reader(BufReader::new(file), &mut plain, None)?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<single>".into());
            members_searched = 1;
            grep_plain(&name, &plain, &re, &mut matches);
        }
    }

    let match_count = matches.len() as u32;
    Ok(GrepResult {
        pattern: pattern.to_string(),
        matches,
        members_searched,
        members_skipped,
        match_count,
    })
}

fn grep_plain(member: &str, plain: &[u8], re: &Regex, matches: &mut Vec<GrepMatch>) {
    // Lossy decode so binary members don't panic; still searchable as text.
    let text = String::from_utf8_lossy(plain);
    for (idx, line) in text.lines().enumerate() {
        if re.is_match(line) {
            matches.push(GrepMatch {
                member: member.to_string(),
                line_number: idx + 1,
                line: line.to_string(),
            });
        }
    }
}

/// Sidecar path for `seal` / `check`: `<archive>.sha256`.
pub fn seal_sidecar_path(archive: &Path) -> PathBuf {
    let mut s = archive.as_os_str().to_os_string();
    s.push(".sha256");
    PathBuf::from(s)
}

/// Hash an archive file and write a password-less integrity sidecar (`.rzst.sha256`).
pub fn seal_archive(path: &Path) -> Result<SealInfo> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = sha256(&data);
    let hex_digest = hex::encode(digest);
    let sidecar = seal_sidecar_path(path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    // sha256sum-compatible: "<hex>  <filename>\n"
    let contents = format!("{hex_digest}  {file_name}\n");
    fs::write(&sidecar, contents.as_bytes())
        .with_context(|| format!("writing sidecar {}", sidecar.display()))?;
    Ok(SealInfo {
        archive: path.to_path_buf(),
        sidecar,
        sha256: hex_digest,
        bytes: data.len() as u64,
    })
}

/// Verify an archive against its `.sha256` sidecar written by [`seal_archive`].
pub fn check_seal(path: &Path) -> Result<SealInfo> {
    let sidecar = seal_sidecar_path(path);
    if !sidecar.exists() {
        bail!(
            "no seal sidecar found: {} (run `rzc seal` first)",
            sidecar.display()
        );
    }
    let text = fs::read_to_string(&sidecar)
        .with_context(|| format!("reading sidecar {}", sidecar.display()))?;
    let expected_hex = parse_sidecar_hash(&text)?;
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let actual = hex::encode(sha256(&data));
    if actual != expected_hex {
        bail!(
            "seal check failed for {}: expected {expected_hex}, got {actual}",
            path.display()
        );
    }
    Ok(SealInfo {
        archive: path.to_path_buf(),
        sidecar,
        sha256: actual,
        bytes: data.len() as u64,
    })
}

fn parse_sidecar_hash(text: &str) -> Result<String> {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .context("seal sidecar is empty")?;
    let hex_part = line
        .split_whitespace()
        .next()
        .context("seal sidecar has no hash")?;
    if hex_part.len() != HASH_LEN * 2 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("seal sidecar hash is not a valid 64-char hex SHA-256");
    }
    Ok(hex_part.to_ascii_lowercase())
}

/// Rewrite a v3 pack archive, dropping members that match any exclude glob.
///
/// Compressed payloads are copied without re-compression. Format remains v3.
pub fn repack_archive(
    input: &Path,
    output: &Path,
    excludes: &[String],
    force: bool,
) -> Result<RepackStats> {
    if output.exists() && !force {
        bail!(
            "output already exists: {} (use --force to overwrite)",
            output.display()
        );
    }
    if input == output {
        bail!("repack input and output paths must differ");
    }

    let exclude_set = build_exclude_set(excludes)?;
    let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut reader = BufReader::new(file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid file magic; this is not an rzc .rzst file");
    }
    let mut version = [0_u8; 1];
    reader.read_exact(&mut version)?;
    if version[0] != VERSION_V3 {
        bail!(
            "repack requires a v3 pack archive (got version {})",
            version[0]
        );
    }
    let mut level_buf = [0_u8; 4];
    reader.read_exact(&mut level_buf)?;
    let level = i32::from_le_bytes(level_buf);
    let mut count_buf = [0_u8; 4];
    reader.read_exact(&mut count_buf)?;
    let file_count = u32::from_le_bytes(count_buf);

    // Buffer kept members so we know the final count before writing the header.
    let mut kept: Vec<(String, u64, [u8; HASH_LEN], Vec<u8>)> = Vec::new();
    let mut excluded = 0_u32;
    let mut original_bytes = 0_u64;

    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        let path_bytes = read_declared(&mut reader, path_len as u64, "pack member path")?;
        let member_path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let compressed = read_declared(&mut reader, compressed_len, "pack member payload")?;

        if is_excluded(&member_path, &exclude_set) {
            excluded += 1;
            continue;
        }
        original_bytes += original_len;
        kept.push((member_path, original_len, checksum, compressed));
    }

    if kept.is_empty() {
        bail!("repack would produce an empty archive (all members excluded)");
    }

    ensure_parent_dir(output)?;
    let out_file =
        File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut writer = BufWriter::new(out_file);
    writer.write_all(MAGIC)?;
    writer.write_all(&[VERSION_V3])?;
    writer.write_all(&level.to_le_bytes())?;
    writer.write_all(&(kept.len() as u32).to_le_bytes())?;

    for (member_path, original_len, checksum, compressed) in &kept {
        let path_bytes = member_path.as_bytes();
        writer.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
        writer.write_all(path_bytes)?;
        writer.write_all(&original_len.to_le_bytes())?;
        writer.write_all(checksum)?;
        writer.write_all(&(compressed.len() as u64).to_le_bytes())?;
        writer.write_all(compressed)?;
    }
    writer.flush()?;
    drop(writer);

    let archive_bytes = fs::metadata(output)
        .with_context(|| format!("reading metadata for {}", output.display()))?
        .len();

    Ok(RepackStats {
        kept: kept.len() as u32,
        excluded,
        original_bytes,
        archive_bytes,
        output_path: output.to_path_buf(),
    })
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

/// Format a duration in a human-readable form (`1.234s`, `2m 03s`, `1h 02m 03s`).
pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs_f64();
    if total_secs < 60.0 {
        if total_secs == 0.0 {
            return "0s".to_string();
        }
        if total_secs < 0.001 {
            return format!("{}µs", d.as_micros().max(1));
        }
        return format!("{total_secs:.3}s");
    }
    let total = d.as_secs();
    let hours = total / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}h {mins:02}m {secs:02}s")
    } else {
        format!("{mins}m {secs:02}s")
    }
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
                original_len: 999,
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

    #[test]
    fn pack_unpack_roundtrip() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("proj");
        fs::create_dir_all(src.join("src"))?;
        fs::write(src.join("README"), b"hello pack world ".repeat(20))?;
        fs::write(src.join("src/main.rs"), b"fn main() {}\n".repeat(30))?;

        let archive = temp.path().join("proj.rzst");
        let stats = pack_directory(&src, &archive, 3, 1, &[])?;
        assert_eq!(stats.file_count, 2);
        assert!(stats.archive_bytes > 0);
        assert!(stats.original_bytes > 0);

        let out = temp.path().join("restored");
        let unpack = unpack_archive(&archive, &out, false)?;
        assert_eq!(unpack.written, 2);
        assert_eq!(unpack.skipped, 0);
        assert_eq!(
            fs::read(out.join("README"))?,
            b"hello pack world ".repeat(20)
        );
        assert_eq!(
            fs::read(out.join("src/main.rs"))?,
            b"fn main() {}\n".repeat(30)
        );

        // list / inspect
        let list = list_archive(&archive)?;
        match list {
            ListResult::Pack(p) => {
                assert_eq!(p.file_count, 2);
                assert_eq!(p.version, VERSION_V3);
                assert_eq!(p.members.len(), 2);
            }
            ListResult::Single(_) => panic!("expected pack"),
        }

        // verify pack
        let n = verify_file(&archive, None)?;
        assert_eq!(n, stats.original_bytes);
        Ok(())
    }

    #[test]
    fn pack_exclude_globs() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("tree");
        fs::create_dir_all(src.join("target/debug"))?;
        fs::create_dir_all(src.join(".git"))?;
        fs::write(src.join("keep.txt"), b"keep me")?;
        fs::write(src.join("target/debug/x.o"), b"object")?;
        fs::write(src.join(".git/config"), b"git")?;

        let archive = temp.path().join("bundle.rzst");
        let excludes = vec!["target".into(), "*.git*".into(), ".git".into()];
        let stats = pack_directory(&src, &archive, 3, 1, &excludes)?;
        assert_eq!(stats.file_count, 1);

        let info = inspect_pack(&archive)?;
        assert_eq!(info.members.len(), 1);
        assert_eq!(info.members[0].path, "keep.txt");
        Ok(())
    }

    #[test]
    fn recursive_exclude() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("target"))?;
        fs::write(root.join("a.txt"), b"aaaa")?;
        fs::write(root.join("target/skip.bin"), b"bbbb")?;

        let stats = compress_dir_recursive_ex(&root, 3, 1, &["target".into()], false, None)?;
        assert_eq!(stats.len(), 1);
        assert!(root.join("a.txt.rzst").is_file());
        assert!(!root.join("target/skip.bin.rzst").exists());
        Ok(())
    }

    #[test]
    fn dry_run_does_not_write() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("data.txt");
        fs::write(&input, b"dry run payload ".repeat(100))?;
        let out = temp.path().join("data.txt.rzst");

        let dry = compress_file_dry_run(&input, 3, 1)?;
        assert!(!out.exists());
        assert_eq!(dry.input_bytes, 16 * 100);
        assert!(dry.estimated_compressed_bytes > 0);
        assert!(dry.estimated_compressed_bytes < dry.input_bytes);

        // Real compress should be same size as estimate
        compress_file(&input, &out, 3, 1, None)?;
        let real = fs::metadata(&out)?.len();
        assert_eq!(real, dry.estimated_compressed_bytes);
        Ok(())
    }

    #[test]
    fn list_single_and_pack() -> Result<()> {
        let temp = tempdir()?;
        let f = temp.path().join("one.txt");
        fs::write(&f, b"single file")?;
        let single = temp.path().join("one.txt.rzst");
        compress_file(&f, &single, 3, 1, None)?;

        match list_archive(&single)? {
            ListResult::Single(info) => {
                assert_eq!(info.header.original_len, 11);
                assert_eq!(info.kind, "single");
            }
            ListResult::Pack(_) => panic!("expected single"),
        }

        let dir = temp.path().join("d");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("x"), b"x")?;
        let pack = temp.path().join("d.rzst");
        pack_directory(&dir, &pack, 3, 1, &[])?;
        match list_archive(&pack)? {
            ListResult::Pack(p) => assert_eq!(p.file_count, 1),
            ListResult::Single(_) => panic!("expected pack"),
        }
        Ok(())
    }

    #[test]
    fn doctor_passes() {
        let report = doctor();
        assert!(report.ok, "{:?}", report.messages);
        assert!(report.zstd_roundtrip);
        assert!(report.container_v2_roundtrip);
        assert!(report.pack_v3_roundtrip);
    }

    #[test]
    fn skip_existing_decompress() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("x.txt");
        let compressed = temp.path().join("x.txt.rzst");
        let output = temp.path().join("out.txt");
        fs::write(&input, b"original")?;
        compress_file(&input, &compressed, 3, 1, None)?;
        fs::write(&output, b"already here")?;

        let stats = decompress_file_opts(&compressed, &output, true, None)?;
        assert_eq!(fs::read(&output)?, b"already here");
        assert_eq!(stats.output_bytes, b"already here".len() as u64);

        let stats = decompress_file_opts(&compressed, &output, false, None)?;
        assert_eq!(fs::read(&output)?, b"original");
        assert_eq!(stats.output_bytes, b"original".len() as u64);
        Ok(())
    }

    #[test]
    fn unpack_skip_existing() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("s");
        fs::create_dir_all(&src)?;
        fs::write(src.join("f.txt"), b"packed")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let out = temp.path().join("o");
        fs::create_dir_all(&out)?;
        fs::write(out.join("f.txt"), b"keep")?;
        let stats = unpack_archive(&archive, &out, true)?;
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.written, 0);
        assert_eq!(fs::read(out.join("f.txt"))?, b"keep");
        Ok(())
    }

    #[test]
    fn path_traversal_rejected() {
        assert!(safe_join(Path::new("/tmp/out"), "../etc/passwd").is_err());
        assert!(safe_join(Path::new("/tmp/out"), "ok/../../etc").is_err());
        assert!(normalize_archive_path(Path::new("a/../b")).is_err());
    }

    #[test]
    fn counting_sink_matches_vec_len() -> Result<()> {
        let data = b"count me ".repeat(200);
        let mut sink = CountingSink::default();
        compress_bytes(&data, &mut sink, 3, 1, None)?;
        let mut vec = Vec::new();
        compress_bytes(&data, &mut vec, 3, 1, None)?;
        assert_eq!(sink.bytes, vec.len() as u64);
        Ok(())
    }

    #[test]
    fn unpack_only_member() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("sub"))?;
        fs::write(src.join("a.txt"), b"aaa")?;
        fs::write(src.join("sub/b.txt"), b"bbb")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let out = temp.path().join("out");
        let stats = unpack_archive_opts(
            &archive,
            &out,
            &UnpackOpts {
                skip_existing: false,
                only: Some("sub/b.txt".into()),
                strip_components: 0,
                force: true,
            },
        )?;
        assert_eq!(stats.written, 1);
        assert!(!out.join("a.txt").exists());
        assert_eq!(fs::read(out.join("sub/b.txt"))?, b"bbb");

        let missing = unpack_archive_opts(
            &archive,
            &temp.path().join("out2"),
            &UnpackOpts {
                only: Some("nope.txt".into()),
                force: true,
                ..Default::default()
            },
        );
        assert!(missing.is_err());
        Ok(())
    }

    #[test]
    fn unpack_strip_components() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("top/nested"))?;
        fs::write(src.join("top/nested/file.txt"), b"payload")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let out = temp.path().join("out");
        let stats = unpack_archive_opts(
            &archive,
            &out,
            &UnpackOpts {
                strip_components: 1,
                force: true,
                ..Default::default()
            },
        )?;
        assert_eq!(stats.written, 1);
        assert_eq!(fs::read(out.join("nested/file.txt"))?, b"payload");
        assert!(!out.join("top").exists());

        let out2 = temp.path().join("out2");
        let stats = unpack_archive_opts(
            &archive,
            &out2,
            &UnpackOpts {
                strip_components: 2,
                force: true,
                ..Default::default()
            },
        )?;
        assert_eq!(stats.written, 1);
        assert_eq!(fs::read(out2.join("file.txt"))?, b"payload");
        Ok(())
    }

    #[test]
    fn strip_path_components_helper() {
        assert_eq!(
            strip_path_components("a/b/c.txt", 0).as_deref(),
            Some("a/b/c.txt")
        );
        assert_eq!(
            strip_path_components("a/b/c.txt", 1).as_deref(),
            Some("b/c.txt")
        );
        assert_eq!(
            strip_path_components("a/b/c.txt", 2).as_deref(),
            Some("c.txt")
        );
        assert_eq!(strip_path_components("a/b/c.txt", 3), None);
        assert_eq!(strip_path_components("file.txt", 1), None);
    }

    #[test]
    fn cat_member_from_pack() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(src.join("hello.txt"), b"hello cat world")?;
        fs::write(src.join("other.txt"), b"other")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let mut out = Vec::new();
        let n = cat_member(&archive, Some("hello.txt"), &mut out)?;
        assert_eq!(n, b"hello cat world".len() as u64);
        assert_eq!(out, b"hello cat world");

        assert!(cat_member(&archive, Some("missing"), &mut Vec::new()).is_err());
        Ok(())
    }

    #[test]
    fn cat_single_file_archive() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("x.txt");
        let compressed = temp.path().join("x.txt.rzst");
        fs::write(&input, b"single cat")?;
        compress_file(&input, &compressed, 3, 1, None)?;

        let mut out = Vec::new();
        let n = cat_member(&compressed, None, &mut out)?;
        assert_eq!(n, 10);
        assert_eq!(out, b"single cat");
        Ok(())
    }

    #[test]
    fn diff_pack_archives() -> Result<()> {
        let temp = tempdir()?;
        let a_dir = temp.path().join("a");
        let b_dir = temp.path().join("b");
        fs::create_dir_all(&a_dir)?;
        fs::create_dir_all(&b_dir)?;
        fs::write(a_dir.join("same.txt"), b"same")?;
        fs::write(b_dir.join("same.txt"), b"same")?;
        fs::write(a_dir.join("only_a.txt"), b"a")?;
        fs::write(b_dir.join("only_b.txt"), b"b")?;
        fs::write(a_dir.join("changed.txt"), b"v1")?;
        fs::write(b_dir.join("changed.txt"), b"v2")?;

        let a = temp.path().join("a.rzst");
        let b = temp.path().join("b.rzst");
        pack_directory(&a_dir, &a, 3, 1, &[])?;
        pack_directory(&b_dir, &b, 3, 1, &[])?;

        let diff = diff_archives(&a, &b)?;
        assert_eq!(diff.identical, 1);
        assert_eq!(diff.only_in_a, 1);
        assert_eq!(diff.only_in_b, 1);
        assert_eq!(diff.changed, 1);
        assert!(!diff.is_equal());

        let same = diff_archives(&a, &a)?;
        assert!(same.is_equal());
        assert_eq!(same.identical, 3);
        Ok(())
    }

    #[test]
    fn diff_single_file_archives() -> Result<()> {
        let temp = tempdir()?;
        let f1 = temp.path().join("one.txt");
        let f2 = temp.path().join("two.txt");
        fs::write(&f1, b"content A")?;
        fs::write(&f2, b"content B")?;
        let a = temp.path().join("one.txt.rzst");
        let b = temp.path().join("two.txt.rzst");
        compress_file(&f1, &a, 3, 1, None)?;
        compress_file(&f2, &b, 3, 1, None)?;

        let diff = diff_archives(&a, &b)?;
        // Different filenames → only_in_a + only_in_b (no shared path)
        assert_eq!(diff.only_in_a + diff.only_in_b + diff.changed, 2);
        assert!(!diff.is_equal());
        Ok(())
    }

    #[test]
    fn force_refuses_overwrite() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("in.txt");
        let out = temp.path().join("out.rzst");
        fs::write(&input, b"data")?;
        compress_file_opts(&input, &out, 3, 1, true, None)?;
        let err = compress_file_opts(&input, &out, 3, 1, false, None)
            .expect_err("should refuse overwrite");
        assert!(err.to_string().contains("--force"));
        compress_file_opts(&input, &out, 3, 1, true, None)?;

        let dir = temp.path().join("d");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("f"), b"x")?;
        let pack_out = temp.path().join("p.rzst");
        pack_directory_opts(
            &dir,
            &pack_out,
            3,
            1,
            &PackOpts {
                force: true,
                ..Default::default()
            },
            None,
        )?;
        let err = pack_directory_opts(
            &dir,
            &pack_out,
            3,
            1,
            &PackOpts {
                force: false,
                ..Default::default()
            },
            None,
        )
        .expect_err("pack should refuse overwrite");
        assert!(err.to_string().contains("--force"));
        Ok(())
    }

    #[test]
    fn pack_newer_than_days() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(&src)?;
        let recent = src.join("recent.txt");
        let old = src.join("old.txt");
        fs::write(&recent, b"new")?;
        fs::write(&old, b"old")?;

        // Age `old` to ~10 days ago.
        let ten_days = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60),
        );
        filetime::set_file_mtime(&old, ten_days)?;

        let archive = temp.path().join("a.rzst");
        let stats = pack_directory_opts(
            &src,
            &archive,
            3,
            1,
            &PackOpts {
                newer_than_days: Some(3),
                force: true,
                ..Default::default()
            },
            None,
        )?;
        assert_eq!(stats.file_count, 1);
        let info = inspect_pack(&archive)?;
        assert_eq!(info.members.len(), 1);
        assert_eq!(info.members[0].path, "recent.txt");
        Ok(())
    }

    #[test]
    fn unpack_force_required_when_exists() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("s");
        fs::create_dir_all(&src)?;
        fs::write(src.join("f.txt"), b"packed")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let out = temp.path().join("o");
        fs::create_dir_all(&out)?;
        fs::write(out.join("f.txt"), b"keep")?;

        let err = unpack_archive_opts(
            &archive,
            &out,
            &UnpackOpts {
                force: false,
                skip_existing: false,
                ..Default::default()
            },
        )
        .expect_err("should require force");
        assert!(err.to_string().contains("--force") || err.to_string().contains("already exists"));
        assert_eq!(fs::read(out.join("f.txt"))?, b"keep");

        let stats = unpack_archive_opts(
            &archive,
            &out,
            &UnpackOpts {
                force: true,
                ..Default::default()
            },
        )?;
        assert_eq!(stats.written, 1);
        assert_eq!(fs::read(out.join("f.txt"))?, b"packed");
        Ok(())
    }

    #[test]
    fn format_path_tree_nested() {
        let paths = vec![
            "README".into(),
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "docs/guide.md".into(),
        ];
        let tree = format_path_tree(&paths);
        assert!(tree.contains("README"));
        assert!(tree.contains("src"));
        assert!(tree.contains("main.rs"));
        assert!(tree.contains("lib.rs"));
        assert!(tree.contains("docs"));
        assert!(tree.contains("├──") || tree.contains("└──"));
    }

    #[test]
    fn archive_tree_from_pack() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("sub"))?;
        fs::write(src.join("a.txt"), b"a")?;
        fs::write(src.join("sub/b.txt"), b"b")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;
        let tree = archive_tree(&archive)?;
        assert!(tree.contains("a.txt"));
        assert!(tree.contains("sub"));
        assert!(tree.contains("b.txt"));
        Ok(())
    }

    #[test]
    fn grep_pack_members() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(src.join("notes.txt"), b"hello world\nfind me here\nbye\n")?;
        fs::write(src.join("other.txt"), b"nothing interesting\n")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let result = grep_archive(&archive, "find me", DEFAULT_GREP_MAX_SIZE)?;
        assert_eq!(result.match_count, 1);
        assert_eq!(result.matches[0].member, "notes.txt");
        assert_eq!(result.matches[0].line_number, 2);
        assert!(result.matches[0].line.contains("find me"));

        let re = grep_archive(&archive, r"find\s+me", DEFAULT_GREP_MAX_SIZE)?;
        assert_eq!(re.match_count, 1);
        Ok(())
    }

    #[test]
    fn grep_skips_oversize_members() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(src.join("big.txt"), b"secret marker in big file")?;
        let archive = temp.path().join("a.rzst");
        pack_directory(&src, &archive, 3, 1, &[])?;

        let result = grep_archive(&archive, "secret", 5)?;
        assert_eq!(result.members_skipped, 1);
        assert_eq!(result.members_searched, 0);
        assert_eq!(result.match_count, 0);
        Ok(())
    }

    #[test]
    fn seal_and_check_roundtrip() -> Result<()> {
        let temp = tempdir()?;
        let input = temp.path().join("x.txt");
        let archive = temp.path().join("x.txt.rzst");
        fs::write(&input, b"seal me")?;
        compress_file(&input, &archive, 3, 1, None)?;

        let sealed = seal_archive(&archive)?;
        assert!(sealed.sidecar.exists());
        assert_eq!(sealed.sidecar, seal_sidecar_path(&archive));

        let checked = check_seal(&archive)?;
        assert_eq!(checked.sha256, sealed.sha256);

        // Tamper with archive
        let mut bytes = fs::read(&archive)?;
        if let Some(last) = bytes.last_mut() {
            *last ^= 0xff;
        }
        fs::write(&archive, &bytes)?;
        assert!(check_seal(&archive).is_err());
        Ok(())
    }

    #[test]
    fn repack_exclude_members() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(src.join("keep.txt"), b"keep")?;
        fs::write(src.join("drop.tmp"), b"tmp")?;
        fs::write(src.join("notes.log"), b"log")?;
        let input = temp.path().join("in.rzst");
        pack_directory(&src, &input, 3, 1, &[])?;
        assert_eq!(inspect_pack(&input)?.file_count, 3);

        let output = temp.path().join("out.rzst");
        let stats = repack_archive(&input, &output, &["*.tmp".into(), "*.log".into()], true)?;
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.excluded, 2);

        let info = inspect_pack(&output)?;
        assert_eq!(info.file_count, 1);
        assert_eq!(info.members[0].path, "keep.txt");

        let err = repack_archive(&input, &output, &["*.tmp".into()], false);
        assert!(err.is_err());
        Ok(())
    }

    /// A hostile pack header can declare any member length. Before bounded
    /// reads, a 69-byte archive claiming a 281 TB payload aborted the process
    /// (`memory allocation of 281474976710655 bytes failed`) instead of
    /// returning an error. Every entry point that parses v3 members must now
    /// fail gracefully on absurd declared lengths.
    #[test]
    fn absurd_declared_lengths_error_instead_of_aborting() -> Result<()> {
        let temp = tempdir()?;
        let archive = temp.path().join("hostile.rzst");

        // RZC1 | v3 | level | file_count=1 | path_len | path | original_len
        // | sha256 | compressed_len = 281 TB, with no payload behind it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(3);
        bytes.extend_from_slice(&3_i32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&5_u32.to_le_bytes());
        bytes.extend_from_slice(b"a.txt");
        bytes.extend_from_slice(&100_u64.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; HASH_LEN]);
        bytes.extend_from_slice(&0x0000_FFFF_FFFF_FFFF_u64.to_le_bytes());
        fs::write(&archive, &bytes)?;

        let dest = temp.path().join("out");
        let err = unpack_archive(&archive, &dest, false).expect_err("unpack must reject");
        assert!(
            format!("{err:#}").contains("remain in the archive"),
            "unexpected error: {err:#}"
        );
        assert!(list_archive(&archive).is_err(), "list must reject");
        assert!(verify_file(&archive, None).is_err(), "verify must reject");
        Ok(())
    }

    #[test]
    fn format_duration_human() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert!(format_duration(Duration::from_millis(250)).contains('s'));
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
    }
}
