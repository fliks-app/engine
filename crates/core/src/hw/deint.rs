//! Hardware deinterlacers: one frame out per frame in, interlaced frames only, like the CPU bwdif.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuDeint {
    Vaapi,
    Qsv,
    Cuda,
    Videotoolbox,
}

impl GpuDeint {
    /// The boot probe that must pass before the filter is used.
    pub fn probe_name(self) -> &'static str {
        match self {
            Self::Vaapi => "deinterlaceVaapi",
            Self::Qsv => "deinterlaceQsv",
            Self::Cuda => "bwdifCuda",
            Self::Videotoolbox => "bwdifVideotoolbox",
        }
    }

    /// A filter of its own; QSV deinterlaces inside `vpp_qsv`.
    pub fn filter(self) -> Option<&'static str> {
        match self {
            Self::Vaapi => Some("deinterlace_vaapi=mode=default:rate=frame:auto=1"),
            Self::Cuda => Some("bwdif_cuda=mode=send_frame:parity=auto:deint=interlaced"),
            Self::Videotoolbox => {
                Some("bwdif_videotoolbox=mode=send_frame:parity=auto:deint=interlaced")
            }
            Self::Qsv => None,
        }
    }

    /// `deinterlace_vaapi` never outputs the first frame of a run (it is only a past reference).
    /// One cloned frame ahead of it takes that place; the shift `loop` adds is taken back first.
    pub fn lead(self, frame_seconds: f64) -> String {
        match self {
            Self::Vaapi => format!(
                "setpts=PTS-{}/TB,loop=loop=1:size=1:start=0,",
                crate::jsnum::js_string(frame_seconds)
            ),
            _ => String::new(),
        }
    }

    /// The `vpp_qsv` options (trailing colon) that deinterlace, empty for the others.
    pub fn vpp_opts(self) -> &'static str {
        match self {
            Self::Qsv => "deinterlace=advanced:rate=frame:",
            _ => "",
        }
    }
}
