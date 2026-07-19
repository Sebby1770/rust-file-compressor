//! Core library for the `rzc` file compressor.
//!
//! Provides the `.rzst` container format (RZC1), zstd compress/decompress,
//! integrity checking (SHA-256), multi-file pack archives (format v3),
//! and helpers for CLI tooling.

use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
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

/// Doctor self-test result.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub zstd_roundtrip: bool,
    pub container_v2_roundtrip: bool,
    pub pack_v3_roundtrip: bool,
    pub messages: Vec<String>,
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

    let mut members = Vec::with_capacity(file_count as usize);
    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        if path_len > 16 * 1024 {
            bail!("pack member path too long ({path_len} bytes)");
        }
        let mut path_bytes = vec![0_u8; path_len];
        reader.read_exact(&mut path_bytes)?;
        let member_path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        // Skip compressed payload.
        io::copy(&mut reader.by_ref().take(compressed_len), &mut io::sink())
            .context("skipping compressed member payload")?;

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
        let mut path_bytes = vec![0_u8; path_len];
        reader.read_exact(&mut path_bytes)?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let mut compressed = vec![0_u8; compressed_len as usize];
        reader
            .read_exact(&mut compressed)
            .with_context(|| format!("reading pack member {i} payload"))?;

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
pub fn pack_directory(
    dir: &Path,
    output: &Path,
    level: i32,
    threads: u32,
    excludes: &[String],
) -> Result<PackStats> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let exclude_set = build_exclude_set(excludes)?;
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
        // Skip nested .rzst by default? Keep them — user may want to pack archives.
        files.push((rel_str, abs.to_path_buf()));
    }

    if files.is_empty() {
        bail!("no files to pack under {}", dir.display());
    }
    if files.len() > u32::MAX as usize {
        bail!("too many files to pack");
    }

    ensure_parent_dir(output)?;
    let out_file =
        File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut writer = BufWriter::new(out_file);

    writer.write_all(MAGIC)?;
    writer.write_all(&[VERSION_V3])?;
    writer.write_all(&level.to_le_bytes())?;
    writer.write_all(&(files.len() as u32).to_le_bytes())?;

    let mut original_bytes: u64 = 0;
    for (rel_path, abs_path) in &files {
        let data = fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;
        let checksum = sha256(&data);
        let compressed = zstd_compress_raw(&data, level, threads)?;
        let path_bytes = rel_path.as_bytes();
        if path_bytes.len() > u32::MAX as usize {
            bail!("path too long: {rel_path}");
        }

        writer.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
        writer.write_all(path_bytes)?;
        writer.write_all(&(data.len() as u64).to_le_bytes())?;
        writer.write_all(&checksum)?;
        writer.write_all(&(compressed.len() as u64).to_le_bytes())?;
        writer.write_all(&compressed)?;

        original_bytes += data.len() as u64;
    }

    writer.flush()?;
    drop(writer);

    let archive_bytes = fs::metadata(output)
        .with_context(|| format!("reading metadata for {}", output.display()))?
        .len();

    Ok(PackStats {
        file_count: files.len() as u32,
        original_bytes,
        archive_bytes,
        output_path: output.to_path_buf(),
    })
}

/// Unpack a v3 archive into `output_dir`.
pub fn unpack_archive(
    archive: &Path,
    output_dir: &Path,
    skip_existing: bool,
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

    let mut members = Vec::with_capacity(file_count as usize);
    let mut written = 0_u32;
    let mut skipped = 0_u32;

    for _ in 0..file_count {
        let mut path_len_buf = [0_u8; 4];
        reader.read_exact(&mut path_len_buf)?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        let mut path_bytes = vec![0_u8; path_len];
        reader.read_exact(&mut path_bytes)?;
        let member_path = String::from_utf8(path_bytes).context("pack member path is not UTF-8")?;

        let mut original_len_buf = [0_u8; 8];
        reader.read_exact(&mut original_len_buf)?;
        let original_len = u64::from_le_bytes(original_len_buf);

        let mut checksum = [0_u8; HASH_LEN];
        reader.read_exact(&mut checksum)?;

        let mut compressed_len_buf = [0_u8; 8];
        reader.read_exact(&mut compressed_len_buf)?;
        let compressed_len = u64::from_le_bytes(compressed_len_buf);

        let mut compressed = vec![0_u8; compressed_len as usize];
        reader
            .read_exact(&mut compressed)
            .with_context(|| format!("reading compressed data for {member_path}"))?;

        let dest = safe_join(output_dir, &member_path)?;
        if skip_existing && dest.exists() {
            members.push(UnpackMemberStats {
                path: member_path,
                original_bytes: original_len,
                skipped: true,
            });
            skipped += 1;
            continue;
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

    Ok(UnpackStats {
        members,
        written,
        skipped,
        output_dir: output_dir.to_path_buf(),
    })
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
}
