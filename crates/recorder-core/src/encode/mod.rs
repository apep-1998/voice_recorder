//! Segment encoding: Opus-in-Ogg writing, finalization, and salvage.

pub mod opus_ogg;

pub use opus_ogg::{
    read_segment_info, salvage_part_file, FinalizedSegment, SegmentInfo, SegmentWriter,
};
