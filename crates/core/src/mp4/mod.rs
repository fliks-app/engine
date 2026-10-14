//! Segment packaging, bytes in, bytes out (`docs/spec/mp4.md`).
mod boxes;
mod cmaf;
mod grid;
mod remux;
mod timeline;

pub use boxes::{BoxRef, boxes, find_box, make_box, read_box_header};
pub use cmaf::{cmaf_rewrite, is_cmaf_rewritten, rewrite_cmaf};
pub use grid::{
    DECODE_TIME_TOLERANCE_SECONDS, KeyframeGrid, SourceTimeline, compute_segment_grid,
    cue_offset_seconds, grid_segment_index, input_seek_seconds, remux_segment_grid, served_origin,
    served_shift, source_timeline,
};
pub use remux::{
    PacketGrid, RawFrame, RemuxAssemblyPlan, RemuxEdits, RemuxRunStart, build_fragment, gops_of,
    parse_frame, plan_boundary_seconds, remux_assembly_plan, remux_edits, remux_run_start,
    seg_name,
};
pub use timeline::{
    DecodeExtent, IdMap, RawSample, TfdtInfo, TrackInfo, first_tfdt, parse_init_tracks,
    parse_trex_defaults, read_init_edits, read_tfdt, retime_fragments, rewrite_segment_tfdt,
    trun_samples, video_decode_extent, with_init_edits,
};

/// Where the host throws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Error {
    /// A buffer read or write out of bounds (Node `ERR_OUT_OF_RANGE`).
    OutOfRange,
    /// The host's own error message.
    Msg(String),
}

impl std::fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange => f.write_str("out of range"),
            Self::Msg(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Mp4Error {}
