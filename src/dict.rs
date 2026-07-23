//! zstd dictionary training for sets of small, similar files.
//!
//! A trained dictionary dramatically improves ratios on many small files that
//! share structure (logs, JSON records, source trees). Payloads compressed
//! with a dictionary are still standard zstd frames inside the unchanged v2
//! `.rzst` container — they simply require the same dictionary to decode.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

/// zstd's canonical default maximum dictionary size (110 KiB).
pub const DEFAULT_MAX_DICT_SIZE: usize = 112_640;

/// Minimum number of sample files required for meaningful training.
pub const MIN_SAMPLE_FILES: usize = 8;

/// Train a zstd dictionary from sample files.
///
/// Directory inputs are expanded recursively to their regular files. Training
/// degenerates on tiny sample sets, so fewer than [`MIN_SAMPLE_FILES`] samples
/// is rejected with an actionable error.
pub fn train_dictionary(inputs: &[PathBuf], max_size: usize) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            for entry in WalkDir::new(input).follow_links(false) {
                let entry = entry.with_context(|| format!("walking {}", input.display()))?;
                if entry.file_type().is_file() {
                    files.push(entry.path().to_path_buf());
                }
            }
        } else if input.is_file() {
            files.push(input.clone());
        } else {
            bail!("{} is not a file or directory", input.display());
        }
    }
    files.sort();

    if files.len() < MIN_SAMPLE_FILES {
        bail!(
            "dictionary training needs at least {MIN_SAMPLE_FILES} sample files; got {}",
            files.len()
        );
    }

    zstd::dict::from_files(&files, max_size).context("training zstd dictionary")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    pub(crate) fn write_json_samples(dir: &Path, count: usize) -> Result<()> {
        fs::create_dir_all(dir)?;
        for i in 0..count {
            let record = format!(
                concat!(
                    "{{\"user\":\"user-{i}\",\"role\":\"admin\",\"active\":true,",
                    "\"quota\":{quota},\"region\":\"eu-west-1\",\"teams\":",
                    "[\"platform\",\"infra\",\"tooling\"],\"note\":",
                    "\"generated fixture record for dictionary training\"}}\n"
                ),
                i = i,
                quota = i * 100,
            );
            fs::write(dir.join(format!("sample-{i:03}.json")), record.repeat(4))?;
        }
        Ok(())
    }

    #[test]
    fn trains_from_similar_samples() -> Result<()> {
        let temp = tempdir()?;
        let samples = temp.path().join("samples");
        write_json_samples(&samples, 40)?;

        let dict = train_dictionary(&[samples], DEFAULT_MAX_DICT_SIZE)?;
        assert!(!dict.is_empty());
        assert!(dict.len() <= DEFAULT_MAX_DICT_SIZE);
        Ok(())
    }

    #[test]
    fn rejects_too_few_samples() -> Result<()> {
        let temp = tempdir()?;
        let samples = temp.path().join("samples");
        write_json_samples(&samples, 3)?;

        let err = train_dictionary(&[samples], DEFAULT_MAX_DICT_SIZE)
            .expect_err("3 samples should be rejected");
        assert!(
            format!("{err:#}").contains("needs at least 8 sample files; got 3"),
            "unexpected error: {err:#}"
        );
        Ok(())
    }
}
