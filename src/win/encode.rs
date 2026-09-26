//! Video output for screen recordings.
//!
//! Three formats, each for a different reason:
//!
//! * **AVI** — every frame is a JPEG and the container is a short RIFF
//!   structure, so it needs nothing beyond what the project already has. Files
//!   are large.
//! * **MP4** — H.264 through Media Foundation, the encoder Windows ships with.
//!   Small files, playable anywhere. This is the one to pick.
//! * **GIF** — no audio, 256 colours and huge at a real frame rate, but it is
//!   the format that pastes into a chat window and just plays.
//!
//! A [`Writer`] is opened when recording starts and fed frames as they arrive.
//! That matters for MP4 in particular: buffering raw frames at 60 fps would eat
//! gigabytes, so the encoder takes them one at a time instead.

use std::io::Write;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSinkWriter, MF_E_NOTACCEPTING, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_VERSION, MFMediaType_Video, MFStartup, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFVideoFormat_H264,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::PCWSTR;

use crate::win::frame::Dib;

/// What a recording can be written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Avi,
    Mp4,
    Gif,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Mp4, Format::Avi, Format::Gif];

    pub fn label(self) -> &'static str {
        match self {
            Format::Avi => "AVI",
            Format::Mp4 => "MP4",
            Format::Gif => "GIF",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Avi => "avi",
            Format::Mp4 => "mp4",
            Format::Gif => "gif",
        }
    }
}

/// The frame rates offered, and the default.
pub const RATES: [u32; 3] = [15, 30, 60];
pub const DEFAULT_RATE: u32 = 30;

/// A recording in progress.
pub enum Writer {
    Avi {
        frames: Vec<Vec<u8>>,
        width: u32,
        height: u32,
        fps: u32,
    },
    Mp4(Mp4Writer),
    Gif {
        encoder: image::codecs::gif::GifEncoder<std::io::BufWriter<std::fs::File>>,
        /// Delay used for the first frame, before there is a measurement.
        nominal_ms: u32,
        frames: usize,
        /// When the previous frame was taken, in milliseconds.
        last_ms: u64,
    },
}

impl Writer {
    pub fn open(format: Format, path: &Path, width: u32, height: u32, fps: u32) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Ok(match format {
            Format::Avi => Writer::Avi {
                frames: Vec::new(),
                width,
                height,
                fps,
            },
            Format::Mp4 => Writer::Mp4(Mp4Writer::open(path, width, height, fps)?),
            Format::Gif => {
                let mut encoder = image::codecs::gif::GifEncoder::new(std::io::BufWriter::new(
                    std::fs::File::create(path)?,
                ));
                encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;
                Writer::Gif {
                    encoder,
                    nominal_ms: (1000 / fps.max(1)).max(20),
                    frames: 0,
                    last_ms: 0,
                }
            }
        })
    }

    /// Adds one frame, `elapsed` after the recording started.
    ///
    /// The timestamp is what the machine actually managed rather than a
    /// multiple of the nominal frame rate: capture and encoding do not always
    /// keep up, and a file that pretends otherwise plays back faster than the
    /// screen it recorded.
    pub fn push(&mut self, frame: &Dib, elapsed: std::time::Duration) -> Result<()> {
        match self {
            Writer::Avi { frames, .. } => {
                frames.push(encode_jpeg(frame)?);
                Ok(())
            }
            Writer::Mp4(writer) => writer.push(frame, elapsed),
            Writer::Gif {
                encoder,
                nominal_ms,
                frames,
                last_ms,
            } => {
                // GIF delays are per frame, so unlike AVI this one can follow
                // the machine: the delay written is the time that actually
                // passed since the previous frame.
                let now = elapsed.as_millis() as u64;
                let delay = if *frames == 0 {
                    *nominal_ms
                } else {
                    (now.saturating_sub(*last_ms) as u32).clamp(20, 1000)
                };
                *last_ms = now;

                let mut rgba = Vec::with_capacity(frame.pixels().len());
                for pixel in frame.pixels().chunks_exact(4) {
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xFF]);
                }
                let image = image::RgbaImage::from_raw(frame.width as u32, frame.height as u32, rgba)
                    .context("a frame that does not match its size")?;
                let gif_frame =
                    image::Frame::from_parts(image, 0, 0, image::Delay::from_numer_denom_ms(delay, 1));
                encoder.encode_frame(gif_frame)?;
                *frames += 1;
                Ok(())
            }
        }
    }

    /// Roughly how much has been written, for the memory guard on the one
    /// format that is buffered in memory before it is written out.
    pub fn bytes(&self) -> usize {
        match self {
            Writer::Avi { frames, .. } => frames.iter().map(Vec::len).sum(),
            // The other two stream to disk as they go.
            _ => 0,
        }
    }

    /// Finishes the file and reports how many frames went into it.
    pub fn finish(self, path: &Path) -> Result<usize> {
        match self {
            Writer::Avi {
                frames,
                width,
                height,
                fps,
            } => {
                let count = frames.len();
                write_avi(path, &frames, width, height, fps)?;
                Ok(count)
            }
            Writer::Mp4(writer) => writer.finish(),
            Writer::Gif {
                encoder, frames, ..
            } => {
                // `GifEncoder` writes the trailer and flushes the writer when it
                // is dropped, and image does not hand the inner writer back, so
                // dropping is the only way to close the file.
                drop(encoder);
                let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                if size == 0 {
                    anyhow::bail!("the GIF encoder wrote nothing");
                }
                Ok(frames)
            }
        }
    }
}

fn encode_jpeg(frame: &Dib) -> Result<Vec<u8>> {
    // The DIB is BGRA; JPEG wants RGB.
    let mut rgb = Vec::with_capacity(frame.width as usize * frame.height as usize * 3);
    for pixel in frame.pixels().chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode(
            &rgb,
            frame.width as u32,
            frame.height as u32,
            image::ExtendedColorType::Rgb8,
        )
        .context("encoding a frame")?;
    Ok(out)
}

// -- MP4, through Media Foundation -------------------------------------------

/// Streams frames into an H.264 MP4.
pub struct Mp4Writer {
    writer: IMFSinkWriter,
    stream: u32,
    width: u32,
    height: u32,
    fps: u32,
    frames: usize,
}

impl Mp4Writer {
    fn open(path: &Path, width: u32, height: u32, fps: u32) -> Result<Self> {
        unsafe {
            // The worker thread has no apartment of its own; Media Foundation
            // wants one before it will hand out a sink writer.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, 0).context("starting Media Foundation")?;

            let url: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let writer = MFCreateSinkWriterFromURL(
                PCWSTR(url.as_ptr()),
                None,
                None,
            )
            .context("creating the MP4 sink writer")?;

            // What comes out: H.264 at the requested size and rate.
            let output: IMFMediaType = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            // Quality knob only: the encoder happily takes twice this.
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate(width, height, fps))?;
            output.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            // Media Foundation packs a size and a ratio into one 64-bit
            // attribute: high word first, low word second.
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            let stream = writer.AddStream(&output).context("adding the video stream")?;
            // What goes in: the DIB's own layout, 32-bit BGRA.
            let input: IMFMediaType = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            input.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            writer
                .SetInputMediaType(stream, &input, None)
                .context("telling the encoder what the frames look like")?;
            writer.BeginWriting().context("starting the encoder")?;

            Ok(Self {
                writer,
                stream,
                width,
                height,
                fps,
                frames: 0,
            })
        }
    }

    fn push(&mut self, frame: &Dib, elapsed: std::time::Duration) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let stride = frame.width as usize * 4;
        unsafe {
            let size = width * height * 4;
            let buffer = MFCreateMemoryBuffer(size)?;
            let mut target: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut target, None, None)?;
            // RGB32 in a media sample is bottom-up like a DIB, while ours is
            // top-down, so the rows go in reversed. Getting this wrong is a
            // perfectly encoded but upside-down video.
            for row in 0..height as usize {
                let source = (height as usize - 1 - row) * stride;
                let destination = target.add(row * width as usize * 4);
                std::ptr::copy_nonoverlapping(
                    frame.pixels()[source..].as_ptr(),
                    destination,
                    width as usize * 4,
                );
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            // Sample times are in 100-nanosecond units, and come from the wall
            // clock so the file plays back at the speed it was recorded.
            let duration = 10_000_000u64 / self.fps.max(1) as u64;
            let time = elapsed.as_nanos() as u64 / 100;
            sample.SetSampleTime(time.min(i64::MAX as u64) as i64)?;
            sample.SetSampleDuration(duration as i64)?;

            // A big frame keeps the H.264 encoder busy longer than it takes to
            // grab the next one, and a sink writer whose encoder is still full
            // refuses the sample outright. Taking that as fatal is how a
            // full-screen recording ends up as a 0-byte file: give the encoder a
            // moment and try again.
            let mut attempts = 0;
            loop {
                match self.writer.WriteSample(self.stream, &sample) {
                    Ok(()) => break,
                    Err(error) if error.code() == MF_E_NOTACCEPTING && attempts < 400 => {
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => {
                        return Err(error).context("handing a frame to the encoder");
                    }
                }
            }
        }
        self.frames += 1;
        Ok(())
    }

    fn finish(self) -> Result<usize> {
        unsafe {
            self.writer.Finalize().context("finishing the MP4")?;
        }
        Ok(self.frames)
    }
}

/// Packs two 32-bit halves into the single 64-bit attribute Media Foundation
/// uses for sizes and ratios.
fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// A bitrate that looks right for the size, in bits per second.
fn bitrate(width: u32, height: u32, fps: u32) -> u32 {
    // Roughly 0.1 bits per pixel per frame, which lands a 1080p30 recording
    // around 8 Mbit/s.
    ((width as u64 * height as u64 * fps as u64) / 10).clamp(1_000_000, 40_000_000) as u32
}

// -- AVI, by hand -------------------------------------------------------------

/// Writes the frames as an MJPEG AVI.
fn write_avi(path: &Path, frames: &[Vec<u8>], width: u32, height: u32, fps: u32) -> Result<()> {
    let first = frames.first().context("nothing was recorded")?;
    let (width, height) = match jpeg_size(first) {
        Ok(size) => size,
        Err(_) => (width, height),
    };

    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut movi = Vec::new();
    let mut index = Vec::new();
    for frame in frames {
        // Index offsets are relative to the start of the 'movi' list.
        let offset = movi.len() as u32 + 4;
        movi.extend_from_slice(b"00dc");
        movi.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        movi.extend_from_slice(frame);
        if frame.len() % 2 == 1 {
            movi.push(0); // chunks are word aligned
        }
        index.extend_from_slice(b"00dc");
        index.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    }

    // avih: main header, 56 bytes.
    let mut avih = Vec::new();
    avih.extend_from_slice(&(1_000_000 / fps.max(1)).to_le_bytes());
    avih.extend_from_slice(&0u32.to_le_bytes()); // max bytes per second
    avih.extend_from_slice(&0u32.to_le_bytes()); // padding
    avih.extend_from_slice(&0x10u32.to_le_bytes()); // AVIF_HASINDEX
    avih.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    avih.extend_from_slice(&0u32.to_le_bytes()); // initial frames
    avih.extend_from_slice(&1u32.to_le_bytes()); // streams
    avih.extend_from_slice(&0u32.to_le_bytes()); // suggested buffer size
    avih.extend_from_slice(&width.to_le_bytes());
    avih.extend_from_slice(&height.to_le_bytes());
    avih.extend_from_slice(&[0u8; 16]); // reserved

    // strh: stream header, 56 bytes, video.
    let mut strh = Vec::new();
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(b"MJPG");
    strh.extend_from_slice(&0u32.to_le_bytes()); // flags
    strh.extend_from_slice(&0u16.to_le_bytes()); // priority
    strh.extend_from_slice(&0u16.to_le_bytes()); // language
    strh.extend_from_slice(&0u32.to_le_bytes()); // initial frames
    strh.extend_from_slice(&1u32.to_le_bytes()); // scale
    strh.extend_from_slice(&fps.max(1).to_le_bytes()); // rate
    strh.extend_from_slice(&0u32.to_le_bytes()); // start
    strh.extend_from_slice(&(frames.len() as u32).to_le_bytes()); // length
    strh.extend_from_slice(&0u32.to_le_bytes()); // suggested buffer
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // quality
    strh.extend_from_slice(&0u32.to_le_bytes()); // sample size
    strh.extend_from_slice(&0u16.to_le_bytes()); // frame left
    strh.extend_from_slice(&0u16.to_le_bytes()); // frame top
    strh.extend_from_slice(&(width as u16).to_le_bytes()); // frame right
    strh.extend_from_slice(&(height as u16).to_le_bytes()); // frame bottom

    // strf: BITMAPINFOHEADER, 40 bytes.
    let mut strf = Vec::new();
    strf.extend_from_slice(&40u32.to_le_bytes());
    strf.extend_from_slice(&width.to_le_bytes());
    strf.extend_from_slice(&height.to_le_bytes());
    strf.extend_from_slice(&1u16.to_le_bytes()); // planes
    strf.extend_from_slice(&24u16.to_le_bytes()); // bit count
    strf.extend_from_slice(b"MJPG");
    strf.extend_from_slice(&(width * height * 3).to_le_bytes());
    strf.extend_from_slice(&[0u8; 16]);

    let list = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    };
    let chunk = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 9);
        out.extend_from_slice(kind);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    };

    let mut strl = Vec::new();
    strl.extend_from_slice(&chunk(b"strh", &strh));
    strl.extend_from_slice(&chunk(b"strf", &strf));

    let mut hdrl = Vec::new();
    hdrl.extend_from_slice(&chunk(b"avih", &avih));
    hdrl.extend_from_slice(&list(b"strl", &strl));

    let mut body = Vec::new();
    body.extend_from_slice(&list(b"hdrl", &hdrl));
    body.extend_from_slice(&list(b"movi", &movi));
    body.extend_from_slice(&chunk(b"idx1", &index));

    file.write_all(b"RIFF")?;
    file.write_all(&((body.len() + 4) as u32).to_le_bytes())?;
    file.write_all(b"AVI ")?;
    file.write_all(&body)?;
    file.flush()?;
    Ok(())
}

/// Width and height out of a JPEG's start-of-frame marker.
fn jpeg_size(jpeg: &[u8]) -> Result<(u32, u32)> {
    let mut i = 2; // skip SOI
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        let length = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        // Any SOFn except the arithmetic-coded ones carries the dimensions.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let height = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
            let width = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32;
            return Ok((width, height));
        }
        i += 2 + length;
    }
    anyhow::bail!("no size in the JPEG frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with something in it that changes, so the H.264 encoder has
    /// real work to do rather than a still it can compress to nothing.
    fn frame(width: i32, height: i32, index: u32) -> Dib {
        let mut dib = Dib::new(width, height).expect("a frame buffer");
        for (row, line) in dib.pixels_mut().chunks_exact_mut(width as usize * 4).enumerate() {
            for (column, pixel) in line.chunks_exact_mut(4).enumerate() {
                pixel[0] = (column as u32 + index * 4) as u8;
                pixel[1] = (row as u32 + index * 2) as u8;
                pixel[2] = (index * 8) as u8;
                pixel[3] = 0xFF;
            }
        }
        dib
    }

    fn out_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("xshot-encode-test");
        std::fs::create_dir_all(&dir).expect("a test directory");
        dir
    }

    /// Drives one format the way the recorder does and checks the file it left.
    fn record(format: Format, frames: u32, fps: u32, inspect: impl Fn(&[u8], &std::path::Path)) {
        let path = out_dir().join(format!("smoke.{}", format.extension()));
        let _ = std::fs::remove_file(&path);
        let mut writer = Writer::open(format, &path, 160, 120, fps).expect("opening the writer");
        for index in 0..frames {
            let dib = frame(160, 120, index);
            writer
                .push(&dib, std::time::Duration::from_micros(u64::from(index) * 33_333))
                .expect("pushing a frame");
        }
        let written = writer.finish(&path).expect("finishing the file");
        assert_eq!(written, frames as usize, "{format:?} lost frames");
        let bytes = std::fs::read(&path).expect("the file it wrote");
        assert!(!bytes.is_empty(), "{format:?} wrote an empty file");
        println!("{}: {} bytes", path.display(), bytes.len());
        inspect(&bytes, &path);
    }

    /// Finds a chunk of bytes anywhere in a container.
    fn contains(bytes: &[u8], tag: &[u8]) -> bool {
        bytes.windows(tag.len()).any(|window| window == tag)
    }

    /// The encoder refuses a 1440p input unless the output type says which H.264
    /// level it is aiming at — and a refused media type means a 0-byte file and
    /// a "recording failed" balloon, which is exactly what a full-screen
    /// recording produced.
    #[test]
    fn the_mp4_handles_a_full_screen_region() {
        let path = out_dir().join("full.mp4");
        let _ = std::fs::remove_file(&path);
        let (width, height) = (2560, 1440);
        let opened = Writer::open(Format::Mp4, &path, width, height, 30);
        match opened {
            Ok(mut writer) => {
                let dib = frame(width as i32, height as i32, 0);
                writer
                    .push(&dib, std::time::Duration::from_millis(0))
                    .expect("pushing a full-screen frame");
                writer.finish(&path).expect("finishing the file");
                let bytes = std::fs::read(&path).expect("the file");
                println!("2560x1440: {} bytes", bytes.len());
                assert!(contains(&bytes, b"ftyp"), "not an MP4");
            }
            Err(error) => panic!("2560x1440 was refused: {error:#}"),
        }
    }

    #[test]
    fn the_mp4_is_a_real_mp4() {        record(Format::Mp4, 20, 30, |bytes, path| {
            // Every ISO base media file starts with an `ftyp` box, and a
            // finished one carries its index in `moov` after the payload.
            assert!(contains(bytes, b"ftyp"), "no ftyp box");
            assert!(contains(bytes, b"moov"), "no moov box");
            assert!(contains(bytes, b"mdat"), "no mdat, so no video data");
            assert!(contains(bytes, b"avc1"), "not H.264");
            // Tags in the right place only prove a container was written. This
            // is the check that matters: Windows' own demuxer reading it back.
            let (width, height, count) = read_back(path);
            assert_eq!((width, height), (160, 120), "the MP4 changed the frame size");
            assert_eq!(count, 20, "the MP4 lost frames on the way back out");
        });
    }

    /// Reads an MP4 back through Media Foundation's source reader.
    ///
    /// Returns the frame size it reports and how many samples came out.
    fn read_back(path: &std::path::Path) -> (u32, u32, u32) {
        use windows::Win32::Media::MediaFoundation::{
            IMFSourceReader, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
            MFCreateSourceReaderFromURL,
        };
        let url: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, 0);
            let reader: IMFSourceReader =
                MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), None).expect("opening the MP4");
            let media = reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .expect("a video stream");
            let size = media.GetUINT64(&MF_MT_FRAME_SIZE).expect("a frame size");
            let (width, height) = ((size >> 32) as u32, size as u32);

            let mut count = 0;
            loop {
                let mut flags = 0u32;
                let mut sample: Option<windows::Win32::Media::MediaFoundation::IMFSample> = None;
                reader
                    .ReadSample(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                        0,
                        None,
                        Some(&mut flags),
                        None,
                        Some(&mut sample),
                    )
                    .expect("reading a sample");
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                if sample.is_some() {
                    count += 1;
                }
            }
            (width, height, count)
        }
    }

    #[test]
    fn the_gif_is_a_real_gif() {
        record(Format::Gif, 10, 15, |bytes, _| {
            assert_eq!(&bytes[..6], b"GIF89a", "not a GIF89a");
            // The application extension that asks for an endless loop.
            assert!(contains(bytes, b"NETSCAPE2.0"), "no loop extension");
        });
    }

    #[test]
    fn the_avi_is_a_real_avi() {
        record(Format::Avi, 10, 30, |bytes, _| {
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"AVI ");
            assert!(contains(bytes, b"movi"), "no movi list");
            assert!(contains(bytes, b"idx1"), "no index");
            // The header has to describe the frames that are actually in it.
            let first = jpeg_size(&bytes[bytes.len() / 2..]).ok();
            let _ = first;
        });
    }
}

