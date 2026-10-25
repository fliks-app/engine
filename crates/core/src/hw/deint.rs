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

    /// A filter of its own; QSV deinterlaces inside `vpp_qsv`. VAAPI uses `bob`: its default mode
    /// drops a run's first frame, shifting the frame-counted keyframe grid.
    pub fn filter(self) -> Option<&'static str> {
        match self {
            Self::Vaapi => Some("deinterlace_vaapi=mode=bob:rate=frame:auto=1"),
            Self::Cuda => Some("bwdif_cuda=mode=send_frame:parity=auto:deint=interlaced"),
            Self::Videotoolbox => {
                Some("bwdif_videotoolbox=mode=send_frame:parity=auto:deint=interlaced")
            }
            Self::Qsv => None,
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
