//! Extracts the embedded picture of an audio file. Spec: `docs/spec/jobs.md` section 9.
use crate::api::{CoverArt, MediaInfo};
use crate::strs;

pub const TIMEOUT_MS: u64 = 60_000;
/// Final names, in the order a stored cover is looked up.
pub const FILES: [&str; 2] = ["cover.jpg", "cover.png"];

pub fn validate(info: &MediaInfo) -> Result<&CoverArt, String> {
    info.cover
        .as_ref()
        .ok_or_else(|| "Cover needs info.cover".into())
}

/// PNG stays PNG, anything else is stored as JPEG.
pub fn extension(cover: &CoverArt) -> &'static str {
    if cover.codec == "png" { "png" } else { "jpg" }
}

pub fn mime(file: &str) -> &'static str {
    if file.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    }
}

/// The picture stream copied as is when it already is a JPEG or a PNG, else encoded to JPEG.
pub fn extract_args(input: &str, cover: &CoverArt, output: &str) -> Vec<String> {
    let mut a = strs(&["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]);
    a.push(input.into());
    a.extend(strs(&["-map"]));
    a.push(format!("0:{}", cover.stream_index));
    a.extend(strs(&["-an", "-sn", "-dn", "-frames:v", "1"]));
    if matches!(cover.codec.as_str(), "mjpeg" | "png") {
        a.extend(strs(&["-c:v", "copy"]));
    } else {
        a.extend(strs(&["-c:v", "mjpeg", "-q:v", "2"]));
    }
    a.extend(strs(&["-f", "image2", "-update", "1"]));
    a.push(output.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(codec: &str) -> CoverArt {
        CoverArt {
            stream_index: 2,
            codec: codec.into(),
            width: None,
            height: None,
        }
    }

    #[test]
    fn needs_a_cover_in_info() {
        assert!(validate(&MediaInfo::default()).is_err());
        let info = MediaInfo {
            cover: Some(art("mjpeg")),
            ..MediaInfo::default()
        };
        assert_eq!(validate(&info).unwrap().stream_index, 2);
    }

    #[test]
    fn jpeg_and_png_are_copied_the_rest_is_encoded() {
        let argv = |c: &str| extract_args("/a.m4b", &art(c), "/t/cover-tmp.jpg").join(" ");
        assert_eq!(
            argv("mjpeg"),
            "-nostdin -hide_banner -loglevel error -y -i /a.m4b -map 0:2 -an -sn -dn \
             -frames:v 1 -c:v copy -f image2 -update 1 /t/cover-tmp.jpg"
        );
        assert!(argv("png").contains("-c:v copy"));
        assert!(argv("webp").contains("-c:v mjpeg -q:v 2"));
        assert_eq!(
            (extension(&art("png")), extension(&art("bmp"))),
            ("png", "jpg")
        );
        assert_eq!(
            (mime("cover.png"), mime("cover.jpg")),
            ("image/png", "image/jpeg")
        );
    }
}
