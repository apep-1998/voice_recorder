//! Opus-in-Ogg segment writer (RFC 7845) with crash-safe finalization.
//!
//! Segments are written to `<name>.opus.part` and renamed to `<name>.opus`
//! on clean finalize (EOS page, flush, fsync, rename). Ogg is page-granular,
//! so a `.part` file left behind by a crash or power loss is still decodable
//! up to its last complete page; [`salvage_part_file`] promotes such files.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};

use crate::error::EncodeError;

/// Opus always frames audio at 48 kHz internally; we also capture at 48 kHz.
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// 20 ms frames: the recommended general-purpose Opus frame size.
pub const FRAMES_PER_PACKET: usize = 960;
/// Maximum size of one encoded Opus packet we allow for.
const MAX_PACKET: usize = 4000;

/// A successfully finalized segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedSegment {
    pub path: PathBuf,
    /// Frames (samples per channel) of real audio written.
    pub n_frames: u64,
}

/// Writes one Ogg/Opus segment file.
pub struct SegmentWriter {
    writer: PacketWriter<'static, BufWriter<File>>,
    encoder: opus::Encoder,
    part_path: PathBuf,
    final_path: PathBuf,
    channels: u8,
    serial: u32,
    /// Buffered samples waiting for a full packet (interleaved).
    pending: Vec<f32>,
    /// Frames of real audio accepted so far.
    n_frames: u64,
    /// Granule position already written (48 kHz samples incl. pre-skip).
    granule: u64,
    /// Packets since the last forced page boundary.
    packets_in_page: u32,
}

/// Force an Ogg page boundary every 50 packets (one second of audio): pages
/// are the crash-recovery granularity, and per-page overhead stays ~0.2%.
const PACKETS_PER_PAGE: u32 = 50;

impl SegmentWriter {
    /// Create `<final_path>.part` and write the Opus headers. `final_path`
    /// must end in `.opus`; parent directories are created as needed.
    pub fn create(final_path: PathBuf, channels: u8, bitrate: u32) -> Result<Self, EncodeError> {
        let opus_channels = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            other => return Err(EncodeError::BadChannels(other)),
        };
        let mut encoder =
            opus::Encoder::new(OPUS_SAMPLE_RATE, opus_channels, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(
            i32::try_from(bitrate).map_err(|_| EncodeError::BadBitrate(bitrate))?,
        ))?;
        let pre_skip = u16::try_from(encoder.get_lookahead()?).unwrap_or(312);

        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| EncodeError::Io {
                path: final_path.clone(),
                source,
            })?;
        }
        let part_path = part_path_for(&final_path);
        let file = File::create(&part_path).map_err(|source| EncodeError::Io {
            path: part_path.clone(),
            source,
        })?;
        let mut writer = PacketWriter::new(BufWriter::new(file));

        // Serial only needs to be unique within concurrently-muxed streams;
        // each segment file holds exactly one stream.
        let serial = 0x5643_5245; // "VCRE"

        let head = opus_head(channels, pre_skip);
        writer
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|source| EncodeError::Io {
                path: part_path.clone(),
                source,
            })?;
        let tags = opus_tags();
        writer
            .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|source| EncodeError::Io {
                path: part_path.clone(),
                source,
            })?;

        Ok(Self {
            writer,
            encoder,
            part_path,
            final_path,
            channels,
            serial,
            pending: Vec::with_capacity(FRAMES_PER_PACKET * usize::from(channels) * 2),
            n_frames: 0,
            granule: u64::from(pre_skip),
            packets_in_page: 0,
        })
    }

    pub fn n_frames(&self) -> u64 {
        self.n_frames
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Append interleaved f32 samples (any length; buffered internally).
    pub fn write_samples(&mut self, interleaved: &[f32]) -> Result<(), EncodeError> {
        self.pending.extend_from_slice(interleaved);
        self.n_frames += interleaved.len() as u64 / u64::from(self.channels);
        let packet_len = FRAMES_PER_PACKET * usize::from(self.channels);
        while self.pending.len() >= packet_len {
            let chunk: Vec<f32> = self.pending.drain(..packet_len).collect();
            self.encode_packet(&chunk)?;
        }
        Ok(())
    }

    /// Flush pending audio, write the end-of-stream page, fsync, and rename
    /// `.part` to the final `.opus` path.
    pub fn finalize(mut self) -> Result<FinalizedSegment, EncodeError> {
        let packet_len = FRAMES_PER_PACKET * usize::from(self.channels);
        let mut last = std::mem::take(&mut self.pending);
        let real_frames = last.len() / usize::from(self.channels);
        // Pad the final packet with silence; the final granule position
        // claims only the real frames, so decoders trim the padding.
        last.resize(packet_len, 0.0);
        self.encode_final_packet(&last, real_frames)?;

        let mut buf_writer = self.writer.into_inner();
        buf_writer.flush().map_err(|source| EncodeError::Io {
            path: self.part_path.clone(),
            source,
        })?;
        let file = buf_writer.into_inner().map_err(|err| EncodeError::Io {
            path: self.part_path.clone(),
            source: err.into_error(),
        })?;
        file.sync_all().map_err(|source| EncodeError::Io {
            path: self.part_path.clone(),
            source,
        })?;
        drop(file);

        std::fs::rename(&self.part_path, &self.final_path).map_err(|source| EncodeError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        Ok(FinalizedSegment {
            path: self.final_path.clone(),
            n_frames: self.n_frames,
        })
    }

    fn encode_packet(&mut self, chunk: &[f32]) -> Result<(), EncodeError> {
        let mut out = vec![0_u8; MAX_PACKET];
        let len = self.encoder.encode_float(chunk, &mut out)?;
        out.truncate(len);
        self.granule += FRAMES_PER_PACKET as u64;
        self.packets_in_page += 1;
        let end_info = if self.packets_in_page >= PACKETS_PER_PAGE {
            self.packets_in_page = 0;
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        self.writer
            .write_packet(out, self.serial, end_info, self.granule)
            .map_err(|source| EncodeError::Io {
                path: self.part_path.clone(),
                source,
            })
    }

    fn encode_final_packet(
        &mut self,
        chunk: &[f32],
        real_frames: usize,
    ) -> Result<(), EncodeError> {
        let mut out = vec![0_u8; MAX_PACKET];
        let len = self.encoder.encode_float(chunk, &mut out)?;
        out.truncate(len);
        self.granule += real_frames as u64;
        self.writer
            .write_packet(
                out,
                self.serial,
                PacketWriteEndInfo::EndStream,
                self.granule,
            )
            .map_err(|source| EncodeError::Io {
                path: self.part_path.clone(),
                source,
            })
    }
}

fn part_path_for(final_path: &Path) -> PathBuf {
    let mut os = final_path.as_os_str().to_owned();
    os.push(".part");
    PathBuf::from(os)
}

/// RFC 7845 identification header.
fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&OPUS_SAMPLE_RATE.to_le_bytes()); // original input rate
    head.extend_from_slice(&0_i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family 0 (mono/stereo)
    head
}

/// RFC 7845 comment header.
fn opus_tags() -> Vec<u8> {
    let vendor = b"voice_recorder";
    let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(
        &u32::try_from(vendor.len())
            .expect("short vendor")
            .to_le_bytes(),
    );
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0_u32.to_le_bytes()); // no user comments
    tags
}

/// Metadata recovered by scanning a segment file's Ogg pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInfo {
    pub channels: u8,
    pub pre_skip: u16,
    /// Frames (samples per channel at 48 kHz) of real audio, i.e. the last
    /// granule position minus pre-skip.
    pub n_frames: u64,
    /// Whether the stream ends with a proper end-of-stream page.
    pub clean_close: bool,
}

/// Scan an Ogg/Opus file (possibly truncated) and recover its metadata.
/// Returns an error if the file does not even contain a valid `OpusHead`.
pub fn read_segment_info(path: &Path) -> Result<SegmentInfo, EncodeError> {
    let file = File::open(path).map_err(|source| EncodeError::Io {
        path: path.to_owned(),
        source,
    })?;
    read_segment_info_from(file, path)
}

fn read_segment_info_from<R: Read + Seek>(
    reader: R,
    path: &Path,
) -> Result<SegmentInfo, EncodeError> {
    let mut packets = PacketReader::new(reader);

    let Ok(Some(head)) = packets.read_packet() else {
        return Err(EncodeError::NotOpus(path.to_owned()));
    };
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err(EncodeError::NotOpus(path.to_owned()));
    }
    let channels = head.data[9];
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]);

    let mut last_granule = 0_u64;
    let mut clean_close = false;
    // Everything after the headers: take granules until the data runs out.
    // A truncated file ends with a read error or EOF — both just stop the
    // scan at the last complete page.
    while let Ok(Some(packet)) = packets.read_packet() {
        let granule = packet.absgp_page();
        // Header pages carry granule 0; audio pages count samples.
        if granule != u64::MAX && granule > last_granule {
            last_granule = granule;
        }
        if packet.last_in_stream() {
            clean_close = true;
            break;
        }
    }

    Ok(SegmentInfo {
        channels,
        pre_skip,
        n_frames: last_granule.saturating_sub(u64::from(pre_skip)),
        clean_close,
    })
}

/// Try to promote a `.opus.part` file left behind by a crash: if it holds a
/// valid `OpusHead` and at least one audio page, rename it to `.opus` and
/// return its recovered info with the final path; otherwise delete it.
pub fn salvage_part_file(part_path: &Path) -> Result<Option<(PathBuf, SegmentInfo)>, EncodeError> {
    let info = match read_segment_info(part_path) {
        Ok(info) if info.n_frames > 0 => info,
        _ => {
            // Headers only (or garbage): nothing worth keeping.
            std::fs::remove_file(part_path).map_err(|source| EncodeError::Io {
                path: part_path.to_owned(),
                source,
            })?;
            return Ok(None);
        }
    };
    let final_path = part_path.with_extension(""); // strips the ".part"
    std::fs::rename(part_path, &final_path).map_err(|source| EncodeError::Io {
        path: final_path.clone(),
        source,
    })?;
    Ok(Some((final_path, info)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_precision_loss)]
    fn sine(frames: usize, channels: usize, freq: f32) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| {
                let frame = (i / channels) as f32;
                (frame * freq * 2.0 * std::f32::consts::PI / OPUS_SAMPLE_RATE as f32).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn writes_and_reads_back_a_clean_segment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg.opus");
        let mut writer = SegmentWriter::create(path.clone(), 1, 32_000).unwrap();

        // 2.5 seconds in odd-sized chunks to exercise buffering.
        let samples = sine(120_000, 1, 440.0);
        for chunk in samples.chunks(1_333) {
            writer.write_samples(chunk).unwrap();
        }
        assert_eq!(writer.n_frames(), 120_000);
        let finalized = writer.finalize().unwrap();
        assert_eq!(finalized.path, path);
        assert_eq!(finalized.n_frames, 120_000);
        assert!(path.exists());
        assert!(!path.with_extension("opus.part").exists());

        let info = read_segment_info(&path).unwrap();
        assert_eq!(info.channels, 1);
        assert_eq!(info.n_frames, 120_000);
        assert!(info.clean_close);
    }

    #[test]
    fn stereo_segment_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.opus");
        let mut writer = SegmentWriter::create(path.clone(), 2, 64_000).unwrap();
        writer.write_samples(&sine(48_000, 2, 330.0)).unwrap();
        writer.finalize().unwrap();
        let info = read_segment_info(&path).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.n_frames, 48_000);
        assert!(info.clean_close);
    }

    #[test]
    fn crashed_segment_is_salvaged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crashed.opus");
        let mut writer = SegmentWriter::create(path.clone(), 1, 32_000).unwrap();
        writer.write_samples(&sine(72_000, 1, 440.0)).unwrap();
        // Simulate a crash: drop without finalize. The BufWriter flushes
        // complete pages as they were written.
        drop(writer);

        let part = dir.path().join("crashed.opus.part");
        assert!(part.exists(), "part file must remain after crash");

        let salvaged = salvage_part_file(&part).unwrap().expect("salvageable");
        let (final_path, info) = salvaged;
        assert_eq!(final_path, path);
        assert!(path.exists());
        assert!(!part.exists());
        assert!(!info.clean_close);
        // Everything up to the last complete page survives. We wrote 75
        // packets; pages close every 50 packets, so exactly the first page
        // group (50 packets = 48000 frames) is recoverable — the rest was
        // still buffered when the "crash" happened.
        assert_eq!(info.n_frames, 48_000);
    }

    #[test]
    fn header_only_part_file_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.opus");
        let writer = SegmentWriter::create(path.clone(), 1, 32_000).unwrap();
        drop(writer);
        let part = dir.path().join("empty.opus.part");
        assert!(part.exists());
        assert_eq!(salvage_part_file(&part).unwrap(), None);
        assert!(!part.exists());
        assert!(!path.exists());
    }

    #[test]
    fn garbage_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.opus");
        std::fs::write(&path, b"this is not an ogg file at all").unwrap();
        assert!(matches!(
            read_segment_info(&path),
            Err(EncodeError::NotOpus(_))
        ));
    }

    #[test]
    fn rejects_unsupported_channel_counts() {
        let dir = tempfile::tempdir().unwrap();
        match SegmentWriter::create(dir.path().join("x.opus"), 3, 32_000) {
            Err(EncodeError::BadChannels(3)) => {}
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("3 channels must be rejected"),
        }
    }
}
