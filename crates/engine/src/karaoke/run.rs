//! What the audio jobs share: outputs written beside their final names, journaled for a boot
//! sweep and published without overwriting, and the typed errors of a missing model or runtime.
use crate::cache::{MERGE_JOURNAL, file_id};
use crate::jobs::Ctx;
use crate::safe_write::{exists, publish, taken};
use fliks_engine_core::api::job_errors;
use std::path::{Path, PathBuf};

struct Claim {
    out: PathBuf,
    tmp: PathBuf,
    id: String,
}

/// The files of one job, claimed together and published together.
pub struct Outputs {
    scratch: PathBuf,
    claims: Vec<Claim>,
}

impl Outputs {
    /// Creates each temporary file exclusively beside its output (none of them may exist yet)
    /// and journals their identities: only those files are ever removed, here or at a boot.
    pub async fn claim(ctx: &Ctx, outs: &[PathBuf]) -> Result<Self, String> {
        let scratch = ctx.rt.cache.merge_dir(&ctx.job.id);
        let mut me = Self {
            scratch,
            claims: vec![],
        };
        let r = me.try_claim(ctx, outs).await;
        match r {
            Ok(()) => Ok(me),
            Err(e) => {
                me.cleanup().await;
                Err(e)
            }
        }
    }

    async fn try_claim(&mut self, ctx: &Ctx, outs: &[PathBuf]) -> Result<(), String> {
        for out in outs {
            if !out.is_absolute() {
                return Err(format!(
                    "not an absolute path on this host: {}",
                    out.display()
                ));
            }
            if exists(out).await {
                return Err(taken(out));
            }
        }
        tokio::fs::create_dir_all(&self.scratch)
            .await
            .map_err(|e| format!("create scratch: {e}"))?;
        for out in outs {
            let (dir, name) = out
                .parent()
                .zip(out.file_name())
                .ok_or("output has no directory")?;
            ctx.check_disk(dir).await?;
            let tmp = dir.join(format!(
                ".{}.merge-{}.tmp",
                name.to_string_lossy(),
                crate::random_hex(4)
            ));
            tokio::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .await
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            let id = file_id(&tmp).unwrap_or_default();
            self.claims.push(Claim {
                out: out.clone(),
                tmp,
                id,
            });
            let text: String = self
                .claims
                .iter()
                .map(|c| {
                    format!(
                        "{}\n{}\n{}\n",
                        c.out.to_string_lossy(),
                        c.tmp.to_string_lossy(),
                        c.id
                    )
                })
                .collect();
            tokio::fs::write(self.scratch.join(MERGE_JOURNAL), text)
                .await
                .map_err(|e| format!("write scratch: {e}"))?;
        }
        Ok(())
    }

    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    pub fn tmp(&self, i: usize) -> &Path {
        &self.claims[i].tmp
    }

    /// Flushes every file to disk and moves each to its output; one that fails takes back the
    /// ones already published.
    pub async fn publish(&self) -> Result<(), String> {
        for c in &self.claims {
            // Write access: Windows refuses to flush a read-only handle.
            let f = tokio::fs::File::options()
                .write(true)
                .open(&c.tmp)
                .await
                .map_err(|e| format!("write {}: {e}", c.tmp.display()))?;
            f.sync_all()
                .await
                .map_err(|e| format!("write {}: {e}", c.tmp.display()))?;
        }
        for (i, c) in self.claims.iter().enumerate() {
            if let Err(e) = publish(&c.tmp, &c.out).await {
                for done in &self.claims[..i] {
                    if file_id(&done.out).as_deref() == Some(done.id.as_str()) {
                        let _ = tokio::fs::remove_file(&done.out).await;
                    }
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Removes the temporary files that are still the claimed ones, and the scratch directory.
    pub async fn cleanup(&self) {
        for c in &self.claims {
            if file_id(&c.tmp).as_deref() == Some(c.id.as_str()) {
                let _ = tokio::fs::remove_file(&c.tmp).await;
            }
        }
        let _ = tokio::fs::remove_dir_all(&self.scratch).await;
    }
}

pub fn unsupported(text: impl std::fmt::Display) -> String {
    job_errors::message(job_errors::UNSUPPORTED, text)
}

/// Sets the flag when dropped: a blocking worker checks it between steps, since a dropped
/// future cannot stop a thread.
pub struct StopOnDrop(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
