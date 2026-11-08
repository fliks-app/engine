//! The `Verify` job: a whole file read, then a counted software decode. Spec: `docs/spec/jobs.md`
//! section 11.
use super::Ctx;
use super::run::{io_err, run_ffmpeg};
use fliks_engine_core::api::{JobErrorClass as Class, JobResult, VerifyMeta, VerifyPayload};
use fliks_engine_core::jobs::optimize as opt;
use std::io::Read;
use std::path::PathBuf;

pub fn validate(p: &VerifyPayload) -> Result<(), String> {
    opt::main_video(&p.info).map(|_| ())
}

pub async fn run(ctx: &Ctx, p: &VerifyPayload) -> Result<JobResult, String> {
    let path = PathBuf::from(ctx.media_path());
    let (c, f) = (ctx.clone(), path.clone());
    // The Rust read classifies access and disk faults without parsing stderr.
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        let mut file = std::fs::File::open(&f)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(total);
            }
            total += n as u64;
            c.pulse();
        }
    })
    .await
    .map_err(|e| Class::Engine.message(e))?
    .map_err(|e| io_err(&path, e))?;
    let duration = p.info.duration_seconds.unwrap_or(0.0);
    // No `-xerror`: the errors are counted, not fatal.
    let argv = opt::decode_args(&path.to_string_lossy(), false);
    let r = run_ffmpeg(ctx, &opt::runnable(argv), duration, (0.0, 99.0)).await?;
    if r.errors > 0 {
        return Err(opt::decode_failure(r.errors, duration, &r.excerpt));
    }
    Ok(JobResult::Verify {
        verify: VerifyMeta {
            bytes: bytes as f64,
            duration_seconds: duration,
        },
    })
}
