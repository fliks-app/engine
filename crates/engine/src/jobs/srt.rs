//! The `SubtitleToSrt` job: one ffmpeg run, SRT text on stdout.
use super::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{JobResult, MediaInfo, SrtSource};
use fliks_engine_core::jobs::srt as pure;
use tokio::io::AsyncReadExt;

pub fn validate(info: &MediaInfo, source: &SrtSource) -> Result<(), String> {
    pure::validate_srt(info, source)
}

pub async fn run(ctx: &Ctx, info: &MediaInfo, source: &SrtSource) -> Result<JobResult, String> {
    pure::validate_srt(info, source)?;
    let media = ctx.media_path();
    let (input, map) = match source {
        SrtSource::File { path } => (path.clone(), None),
        SrtSource::Stream { index } => (media, Some(*index)),
    };
    let argv = pure::to_srt_argv(&input, map);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes { stdout: true });
    let srt = ctx
        .pipeline(pure::TIMEOUT_MS, vec![cmd], |p| async move {
            let ff = &p[0];
            let mut out = ff.take_stdout().ok_or("ffmpeg produced no stdout")?;
            let mut buf = Vec::new();
            out.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
            match ff.wait().await.code {
                Some(0) => String::from_utf8(buf).map_err(|_| "ffmpeg SRT is not UTF-8".into()),
                code => Err(format!("ffmpeg exited with {code:?}: {}", tail(ff, 2))),
            }
        })
        .await??;
    Ok(JobResult::SubtitleToSrt { srt })
}
