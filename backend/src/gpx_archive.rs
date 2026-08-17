use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use flate2::{Compression, write::GzEncoder};

static SUBMISSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub enum ArchiveCategory {
    Valid,
    Error,
}

impl ArchiveCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Error => "error",
        }
    }
}

#[derive(Debug)]
pub struct ArchiveRecord {
    pub path: PathBuf,
    pub compressed_bytes: u64,
}

#[derive(Clone)]
pub struct GpxArchive {
    root: PathBuf,
    retention: Duration,
    max_bytes: u64,
    operation_lock: Arc<Mutex<()>>,
}

impl GpxArchive {
    pub fn new(root: PathBuf, retention: Duration, max_bytes: u64) -> Self {
        Self {
            root,
            retention,
            max_bytes,
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn prepare(&self) -> Result<()> {
        let archive = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = archive
                .operation_lock
                .lock()
                .map_err(|_| anyhow!("GPX archive lock is poisoned"))?;
            ensure_directories(&archive.root)?;
            prune_archives(&archive.root, archive.retention, archive.max_bytes, 0)
        })
        .await
        .context("GPX archive preparation task failed")?
    }

    pub async fn archive(
        &self,
        submission_id: &str,
        original_filename: &str,
        bytes: &[u8],
        category: ArchiveCategory,
    ) -> Result<ArchiveRecord> {
        let archive = self.clone();
        let submission_id = submission_id.to_owned();
        let original_filename = original_filename.to_owned();
        let bytes = bytes.to_vec();

        tokio::task::spawn_blocking(move || {
            let _guard = archive
                .operation_lock
                .lock()
                .map_err(|_| anyhow!("GPX archive lock is poisoned"))?;
            archive_sync(
                &archive.root,
                archive.retention,
                archive.max_bytes,
                &submission_id,
                &original_filename,
                &bytes,
                category,
            )
        })
        .await
        .context("GPX archive task failed")?
    }
}

pub fn submission_id(gpx_hash: &str) -> String {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
    let counter = SUBMISSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hash_prefix = &gpx_hash[..gpx_hash.len().min(12)];
    format!("{timestamp}-{counter:06}-{hash_prefix}")
}

pub fn safe_filename(filename: &str) -> String {
    let basename = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("route.gpx");
    let stem = if basename.to_ascii_lowercase().ends_with(".gpx") {
        &basename[..basename.len() - 4]
    } else {
        basename
    };
    let mut sanitized = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(92)
        .collect::<String>();

    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        sanitized = "route".to_owned();
    }
    format!("{sanitized}.gpx")
}

fn archive_sync(
    root: &Path,
    retention: Duration,
    max_bytes: u64,
    submission_id: &str,
    original_filename: &str,
    bytes: &[u8],
    category: ArchiveCategory,
) -> Result<ArchiveRecord> {
    ensure_directories(root)?;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).context("failed to compress GPX")?;
    let compressed = encoder
        .finish()
        .context("failed to finish GPX compression")?;
    let compressed_bytes =
        u64::try_from(compressed.len()).context("compressed GPX is too large")?;
    if compressed_bytes > max_bytes {
        bail!("compressed GPX size {compressed_bytes} exceeds archive quota {max_bytes}");
    }

    prune_archives(root, retention, max_bytes, compressed_bytes)?;

    let directory = root.join(category.as_str());
    let filename = format!("{submission_id}-{}.gz", safe_filename(original_filename));
    let final_path = unique_path(&directory, &filename);
    let temporary_path = directory.join(format!(
        ".{}.tmp",
        final_path.file_name().unwrap().to_string_lossy()
    ));

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .with_context(|| format!("failed to create {}", temporary_path.display()))?;
        file.write_all(&compressed)
            .with_context(|| format!("failed to write {}", temporary_path.display()))?;
        file.sync_data()
            .with_context(|| format!("failed to sync {}", temporary_path.display()))?;
        fs::rename(&temporary_path, &final_path).with_context(|| {
            format!(
                "failed to move GPX archive from {} to {}",
                temporary_path.display(),
                final_path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result?;

    Ok(ArchiveRecord {
        path: final_path,
        compressed_bytes,
    })
}

fn ensure_directories(root: &Path) -> Result<()> {
    for category in [ArchiveCategory::Valid, ArchiveCategory::Error] {
        let directory = root.join(category.as_str());
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create GPX archive directory {}",
                directory.display()
            )
        })?;
    }
    Ok(())
}

#[derive(Debug)]
struct StoredArchive {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn prune_archives(root: &Path, retention: Duration, max_bytes: u64, incoming: u64) -> Result<()> {
    let now = SystemTime::now();
    let mut archives = Vec::new();

    for category in [ArchiveCategory::Valid, ArchiveCategory::Error] {
        let directory = root.join(category.as_str());
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to inspect {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }

            let filename = entry.file_name();
            let filename = filename.to_string_lossy();
            if filename.starts_with('.') && filename.ends_with(".tmp") {
                fs::remove_file(&path).with_context(|| {
                    format!("failed to remove stale archive {}", path.display())
                })?;
                continue;
            }
            if !filename.ends_with(".gpx.gz") {
                continue;
            }

            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or_default() > retention {
                fs::remove_file(&path).with_context(|| {
                    format!("failed to remove expired archive {}", path.display())
                })?;
                continue;
            }
            archives.push(StoredArchive {
                path,
                size: metadata.len(),
                modified,
            });
        }
    }

    archives.sort_by_key(|archive| archive.modified);
    let mut total = archives.iter().map(|archive| archive.size).sum::<u64>();
    for archive in archives {
        if total.saturating_add(incoming) <= max_bytes {
            break;
        }
        fs::remove_file(&archive.path)
            .with_context(|| format!("failed to remove old archive {}", archive.path.display()))?;
        total = total.saturating_sub(archive.size);
    }

    if total.saturating_add(incoming) > max_bytes {
        bail!("GPX archive quota cannot accommodate the new file");
    }
    Ok(())
}

fn unique_path(directory: &Path, filename: &str) -> PathBuf {
    let candidate = directory.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    for suffix in 1_u32.. {
        let candidate_name = filename
            .strip_suffix(".gpx.gz")
            .map(|stem| format!("{stem}.{suffix}.gpx.gz"))
            .unwrap_or_else(|| format!("{filename}.{suffix}"));
        let candidate = directory.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Read, thread};

    use flate2::read::GzDecoder;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn sanitizes_untrusted_filenames() {
        assert_eq!(
            safe_filename("../../My ride (final).gpx"),
            "My_ride__final_.gpx"
        );
        assert_eq!(safe_filename(".."), "route.gpx");
        assert_eq!(safe_filename("RIDE.GPX"), "RIDE.gpx");
    }

    #[tokio::test]
    async fn archives_and_decompresses_original_bytes() {
        let temp = TempDir::new().unwrap();
        let archive = GpxArchive::new(temp.path().to_path_buf(), Duration::from_secs(60), 1024);
        let contents = b"<gpx>route</gpx>";
        let record = archive
            .archive("submission", "ride.gpx", contents, ArchiveCategory::Valid)
            .await
            .unwrap();

        assert!(record.path.starts_with(temp.path().join("valid")));
        let mut decoder = GzDecoder::new(fs::File::open(record.path).unwrap());
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, contents);
    }

    #[tokio::test]
    async fn prunes_expired_archives() {
        let temp = TempDir::new().unwrap();
        let archive = GpxArchive::new(temp.path().to_path_buf(), Duration::ZERO, 1024);
        archive
            .archive("old", "old.gpx", b"old", ArchiveCategory::Error)
            .await
            .unwrap();
        thread::sleep(Duration::from_millis(2));
        archive.prepare().await.unwrap();

        assert_eq!(fs::read_dir(temp.path().join("error")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn quota_removes_oldest_archives_across_categories() {
        let temp = TempDir::new().unwrap();
        let archive = GpxArchive::new(temp.path().to_path_buf(), Duration::from_secs(60), 50);
        archive
            .archive("first", "first.gpx", b"first route", ArchiveCategory::Valid)
            .await
            .unwrap();
        thread::sleep(Duration::from_millis(2));
        archive
            .archive(
                "second",
                "second.gpx",
                b"second route",
                ArchiveCategory::Error,
            )
            .await
            .unwrap();

        let files = ["valid", "error"]
            .iter()
            .flat_map(|category| fs::read_dir(temp.path().join(category)).unwrap())
            .count();
        assert_eq!(files, 1);
    }

    #[tokio::test]
    async fn concurrent_submissions_create_unique_archives() {
        let temp = TempDir::new().unwrap();
        let archive = GpxArchive::new(temp.path().to_path_buf(), Duration::from_secs(60), 4096);
        let mut tasks = Vec::new();
        for index in 0..8 {
            let archive = archive.clone();
            tasks.push(tokio::spawn(async move {
                archive
                    .archive(
                        "same-submission",
                        "ride.gpx",
                        format!("route {index}").as_bytes(),
                        ArchiveCategory::Valid,
                    )
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(fs::read_dir(temp.path().join("valid")).unwrap().count(), 8);
    }
}
