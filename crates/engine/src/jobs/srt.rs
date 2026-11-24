//! The `SubtitleToSrt` job: one ffmpeg run, SRT text on stdout.
use super::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use crate::runtime::Runtime;
use fliks_engine_core::api::{JobResult, MediaInfo, SrtSource};
use fliks_engine_core::jobs::srt as pure;
use tokio::io::AsyncReadExt;

pub fn validate(rt: &Runtime, info: &MediaInfo, source: &SrtSource) -> Result<(), String> {
    pure::validate_srt(info, source)?;
    input(rt, String::new(), source).map(drop)
}

/// The ffmpeg input and stream map of a source, a sidecar on this host.
fn input(rt: &Runtime, media: String, source: &SrtSource) -> Result<(String, Option<u32>), String> {
    match source {
        SrtSource::File { path } => rt
            .map_absolute(path)
            .map(|p| (p, None))
            .ok_or_else(|| "SubtitleToSrt file path must be absolute".into()),
        SrtSource::Stream { index } => Ok((media, Some(*index))),
    }
}

pub async fn run(ctx: &Ctx, info: &MediaInfo, source: &SrtSource) -> Result<JobResult, String> {
    pure::validate_srt(info, source)?;
    let media = ctx.media_path();
    let (input, map) = input(&ctx.rt, media, source)?;
    let argv = pure::to_srt_argv(&input, map);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes::stdout());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::FOREIGN;

    #[test]
    fn a_sidecar_is_opened_at_its_mapped_path() {
        let (from, to, path) = FOREIGN;
        let rt = Runtime::with_mapping(from, to);
        let sep = std::path::MAIN_SEPARATOR;
        let file = SrtSource::File { path: path.into() };
        assert_eq!(
            input(&rt, "m".into(), &file).unwrap(),
            (format!("{to}{sep}x.mkv"), None)
        );
        let rel = SrtSource::File {
            path: "x.srt".into(),
        };
        assert!(input(&rt, "m".into(), &rel).is_err());
        assert_eq!(
            input(&rt, "m".into(), &SrtSource::Stream { index: 2 }).unwrap(),
            ("m".into(), Some(2))
        );
    }
}
