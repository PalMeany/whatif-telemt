//! Hash-chained audit log.
//!
//! Every mutating panel action appends one JSON line whose hash covers both the
//! record and its predecessor's hash. Deleting or editing a line therefore
//! breaks verification from that point on, which is the property that makes the
//! log worth keeping at all: an append-only file an operator can also rewrite
//! proves nothing.
//!
//! Submodules:
//! - `record`: the record shape, its field bounds, and the chain hash

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tracing::warn;

mod record;

pub(crate) use record::{AuditEntry, AuditRecord, AuditVerification};
use record::{GENESIS, chain_hash, seal};

/// Append-only audit log with a persisted chain head.
pub(crate) struct AuditLog {
    path: PathBuf,
    max_bytes: u64,
    retention_days: u64,
    state: tokio::sync::Mutex<ChainHead>,
}

/// The chain head carried between appends.
struct ChainHead {
    seq: u64,
    hash: String,
}

impl AuditLog {
    /// Opens the log, recovering the chain head from the existing file.
    pub(crate) async fn open(path: PathBuf, max_bytes: u64, retention_days: u64) -> Self {
        let head = match read_head(&path).await {
            Some(head) => head,
            None => ChainHead {
                seq: 0,
                hash: GENESIS.to_string(),
            },
        };
        Self {
            path,
            max_bytes,
            retention_days,
            state: tokio::sync::Mutex::new(head),
        }
    }

    /// Appends one record, rotating the file first when it has grown too large.
    ///
    /// A failure here is reported and swallowed: an audit write that cannot
    /// land must not turn an otherwise successful operation into an error the
    /// operator has to retry, and the failure itself is visible in the log
    /// stream.
    pub(crate) async fn append(&self, entry: AuditEntry, now: u64) {
        let mut head = self.state.lock().await;
        self.rotate_if_needed(&mut head).await;
        let seq = head.seq + 1;
        let record = seal(entry, seq, now, head.hash.clone());
        let mut line = match serde_json::to_vec(&record) {
            Ok(line) => line,
            Err(error) => {
                warn!(%error, "Failed to encode panel audit record");
                return;
            }
        };
        line.push(b'\n');
        match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&line).await {
                    warn!(%error, "Failed to append panel audit record");
                    return;
                }
                if let Err(error) = file.flush().await {
                    warn!(%error, "Failed to flush panel audit record");
                    return;
                }
                head.seq = seq;
                head.hash = record.hash;
            }
            Err(error) => {
                warn!(path = %self.path.display(), %error, "Failed to open panel audit log");
            }
        }
        super::store::restrict_file(&self.path).await;
    }

    /// Reads the most recent records, newest first.
    ///
    /// Rotation is walked backwards when the current segment holds fewer
    /// records than asked for, so a rotation does not make the log look empty.
    pub(crate) async fn tail(&self, limit: usize) -> Vec<AuditRecord> {
        let mut records = Vec::with_capacity(limit.min(1_024));
        for segment in self.segments().await.into_iter().rev() {
            let Ok(content) = tokio::fs::read_to_string(&segment).await else {
                continue;
            };
            let mut segment_records: Vec<AuditRecord> = content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            segment_records.reverse();
            for record in segment_records {
                if records.len() >= limit {
                    return records;
                }
                records.push(record);
            }
        }
        records
    }

    /// Recomputes the chain across every segment and reports where it breaks.
    ///
    /// Rotation preserves the chain, so verification has to start at the oldest
    /// retained segment: verifying only the current file would report every
    /// rotation as tampering.
    pub(crate) async fn verify(&self) -> AuditVerification {
        let mut previous = GENESIS.to_string();
        let mut checked = 0u64;
        let segments = self.segments().await;
        let mut first_segment = true;
        for segment in segments {
            let Ok(content) = tokio::fs::read_to_string(&segment).await else {
                continue;
            };
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(record) = serde_json::from_str::<AuditRecord>(line) else {
                    return AuditVerification {
                        checked,
                        valid: false,
                        broken_at: Some(checked + 1),
                    };
                };
                // Retention deletes whole segments from the front of the chain,
                // so the first record of the oldest retained segment adopts its
                // recorded predecessor instead of being checked against genesis.
                if checked == 0 && first_segment && record.prev != previous {
                    previous = record.prev.clone();
                }
                checked += 1;
                let expected = chain_hash(&record, &previous);
                if record.prev != previous || record.hash != expected {
                    return AuditVerification {
                        checked,
                        valid: false,
                        broken_at: Some(record.seq),
                    };
                }
                previous = record.hash;
            }
            first_segment = false;
        }
        AuditVerification {
            checked,
            valid: true,
            broken_at: None,
        }
    }

    /// Lists every retained segment in chain order, current file last.
    async fn segments(&self) -> Vec<PathBuf> {
        let mut rotated: Vec<(u64, PathBuf)> = Vec::new();
        if let Some(directory) = self.path.parent()
            && let Some(stem) = self
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
            && let Ok(mut entries) = tokio::fs::read_dir(directory).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path == self.path {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(sequence) = rotated_sequence(&name, &stem) {
                    rotated.push((sequence, path));
                }
            }
        }
        rotated.sort_by_key(|(sequence, _)| *sequence);
        let mut segments: Vec<PathBuf> = rotated.into_iter().map(|(_, path)| path).collect();
        segments.push(self.path.clone());
        segments
    }

    /// Rotates the log when it has grown past the configured ceiling.
    ///
    /// The chain head survives rotation, so verification of the current file
    /// starts from the rotated file's last hash rather than from genesis.
    async fn rotate_if_needed(&self, head: &mut ChainHead) {
        let Ok(metadata) = tokio::fs::metadata(&self.path).await else {
            return;
        };
        if metadata.len() < self.max_bytes {
            return;
        }
        let rotated = self.path.with_extension(format!("{}.jsonl", head.seq));
        if let Err(error) = tokio::fs::rename(&self.path, &rotated).await {
            warn!(%error, "Failed to rotate panel audit log");
            return;
        }
        self.prune_rotated().await;
    }

    /// Deletes rotated files older than the retention window.
    async fn prune_rotated(&self) {
        let Some(directory) = self.path.parent() else {
            return;
        };
        let Some(stem) = self
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            return;
        };
        let cutoff = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
            self.retention_days.saturating_mul(86_400),
        ));
        let Some(cutoff) = cutoff else {
            return;
        };
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&stem) || entry.path() == self.path {
                continue;
            }
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified < cutoff && tokio::fs::remove_file(entry.path()).await.is_err() {
                warn!(path = %entry.path().display(), "Failed to prune rotated panel audit log");
            }
        }
    }
}

/// Parses the sequence number out of a rotated segment's file name.
///
/// Rotation names a segment after the chain head it closed at, so the numeric
/// component orders the segments exactly as the chain runs.
fn rotated_sequence(name: &str, stem: &str) -> Option<u64> {
    name.strip_prefix(stem)?
        .strip_prefix('.')?
        .strip_suffix(".jsonl")?
        .parse()
        .ok()
}

/// Recovers the chain head from the tail of an existing log.
async fn read_head(path: &Path) -> Option<ChainHead> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let last = content.lines().rfind(|line| !line.trim().is_empty())?;
    let record: AuditRecord = serde_json::from_str(last).ok()?;
    Some(ChainHead {
        seq: record.seq,
        hash: record.hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(action: &str) -> AuditEntry {
        AuditEntry {
            actor: "root".to_string(),
            actor_id: "op-1".to_string(),
            action: action.to_string(),
            target: "alice".to_string(),
            node: "local".to_string(),
            result: "ok".to_string(),
            address: "203.0.113.5".to_string(),
            detail: String::new(),
        }
    }

    #[tokio::test]
    async fn the_chain_verifies_and_survives_a_reopen() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("audit.jsonl");
        let log = AuditLog::open(path.clone(), 1 << 20, 30).await;
        log.append(entry("user.create"), 1_000).await;
        log.append(entry("user.delete"), 1_001).await;
        let verification = log.verify().await;
        assert!(verification.valid);
        assert_eq!(verification.checked, 2);

        let reopened = AuditLog::open(path.clone(), 1 << 20, 30).await;
        reopened.append(entry("user.patch"), 1_002).await;
        let verification = reopened.verify().await;
        assert!(verification.valid, "{verification:?}");
        assert_eq!(verification.checked, 3);
        let tail = reopened.tail(2).await;
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].action, "user.patch");
        assert_eq!(tail[0].seq, 3);
    }

    #[tokio::test]
    async fn an_edited_record_breaks_verification() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("audit.jsonl");
        let log = AuditLog::open(path.clone(), 1 << 20, 30).await;
        log.append(entry("user.create"), 1_000).await;
        log.append(entry("user.delete"), 1_001).await;

        let content = tokio::fs::read_to_string(&path).await.expect("read");
        let tampered = content.replace("\"target\":\"alice\"", "\"target\":\"mallory\"");
        tokio::fs::write(&path, tampered).await.expect("write");

        let verification = log.verify().await;
        assert!(!verification.valid);
        assert_eq!(verification.broken_at, Some(1));
    }

    #[tokio::test]
    async fn rotation_keeps_the_chain_continuous() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("audit.jsonl");
        let log = AuditLog::open(path.clone(), 64, 30).await;
        for index in 0..4u64 {
            log.append(entry("user.create"), 1_000 + index).await;
        }
        // The file rotated at least once, and the chain still verifies end to
        // end across every retained segment.
        assert!(
            directory.path().join("audit.1.jsonl").exists()
                || directory.path().join("audit.2.jsonl").exists(),
            "expected a rotated segment"
        );
        let verification = log.verify().await;
        assert!(verification.valid, "{verification:?}");
        assert_eq!(verification.checked, 4);
        let tail = log.tail(8).await;
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0].seq, 4);
        assert_eq!(tail[3].seq, 1);
    }

    #[tokio::test]
    async fn verification_survives_a_pruned_oldest_segment() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("audit.jsonl");
        let log = AuditLog::open(path.clone(), 64, 30).await;
        for index in 0..6u64 {
            log.append(entry("user.create"), 1_000 + index).await;
        }
        // Retention removes whole segments from the front of the chain; the
        // remainder still has to verify against its own recorded predecessor.
        let mut segments = log.segments().await;
        segments.pop();
        if let Some(oldest) = segments.first() {
            tokio::fs::remove_file(oldest).await.expect("remove");
        }
        let verification = log.verify().await;
        assert!(verification.valid, "{verification:?}");
    }
}
