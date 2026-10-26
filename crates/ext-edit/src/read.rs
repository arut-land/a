//! `read`: a file as the model reads it. Text comes back as text, a window
//! of its lines if asked; an image comes back as the image, held as a blob
//! the result references (the log holds the reference, never the bytes),
//! and is shown to the model inside the result where its provider reads it
//! there, else after it.
//!
//! A file is an image when its first bytes name a format read here and its
//! header reads as one; anything else is text, if it is UTF-8. An image
//! every provider reads as it is (PNG, JPEG, WebP) within their limits, and
//! upright, is sent as the file's own bytes, undecoded. Any other (GIF, its
//! first frame; BMP; a larger or rotated one) is decoded once, scaled to fit
//! as it is decoded where the format allows (a PNG a row at a time, a JPEG
//! by its DCT), turned upright, and encoded as PNG when it has alpha, else
//! as JPEG. `docs/perf/read-images.md` has the measurements.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read as _, Seek, Write as _};
use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader};
use protocol::{BlobOrigin, BlobRef, Card, Held};
use rt::BlockingClass::{Cpu, Storage};
use serde::{Deserialize, Serialize};

use ext::{Concurrency, Cx, Json, Replay, Schema, Tool};

pub struct Read;

#[derive(Deserialize, Schema)]
pub struct ReadInput {
    /// Relative to the working directory, or absolute.
    pub path: String,
    /// The first line to read, from 1; for text.
    pub offset: Option<usize>,
    /// The most lines to read; for text.
    pub limit: Option<usize>,
}

/// What a call read.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Contents {
    Text(Text),
    Image(Image),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    pub path: PathBuf,
    pub text: String,
    /// The first line `text` holds, from 1.
    pub from: usize,
    /// The lines `text` holds.
    pub lines: usize,
    /// The lines the file holds.
    pub total: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub path: PathBuf,
    /// What the model is sent.
    pub sent: Picture,
    /// The file as it was, when what is sent was converted from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted: Option<Picture>,
    /// The call gave `offset` or `limit`, which an image has no lines for.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub window_ignored: bool,
    /// Where the bytes sent are: the call's result holds it, so the log
    /// records it once, there.
    #[serde(skip)]
    pub blob: Option<BlobRef>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picture {
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// The most text read at once: past it, a window of lines.
const TEXT_LIMIT: u64 = 8 << 20;
/// The longest edge an image is sent with: within what every provider
/// takes whole, Anthropic's cap for many images among them.
const MAX_EDGE: u32 = 2000;
/// The most bytes an image is sent as: 5 MB as base64, the smallest
/// provider's cap (Bedrock's).
const MAX_BYTES: usize = 3_750_000;
/// The most an image scaled to fit holds, decoded.
const SCALED: usize = MAX_EDGE as usize * MAX_EDGE as usize * 4;
/// The largest image file read: what the decoders allocate at most
/// (`image`'s own limit), past which a file is not one worth decoding.
const IMAGE_LIMIT: u64 = 512 << 20;
/// The bytes a format is known by.
const HEAD: usize = 32;

/// The formats read as images: what the providers read, and BMP.
const IMAGES: [ImageFormat; 5] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::WebP,
    ImageFormat::Gif,
    ImageFormat::Bmp,
];
/// The formats every provider reads as they are; GIF is converted, as a
/// provider reads only its first frame, and not every one reads it at all.
const SENT: [ImageFormat; 3] = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP];

impl Tool for Read {
    type In = ReadInput;
    type Out = Contents;

    const NAME: &'static str = "read";
    const DESCRIPTION: &'static str = "Reads a file. Text comes back as text, from line `offset` (from 1) for at most `limit` lines when given; a file past 8 MiB needs them. An image (PNG, JPEG, WebP, GIF, BMP) comes back as the image, scaled to fit if it is large.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &ReadInput, cx: &Cx) -> Result<Contents, String> {
        let path = cx.dir().join(&input.path);
        let shown = &input.path;
        let rt = cx
            .service::<dyn rt::Runtime>()
            .ok_or("the engine provides no runtime")?;
        let fail = |e: io::Error| format!("{shown}: {e}");
        let file = path.clone();
        let (size, header) = offload(rt, Storage, HEAD, move || peek(&file))
            .await
            .and_then(|peeked| peeked)
            .map_err(fail)?;
        let window = (input.offset, input.limit);
        if let Some(header) = header {
            if size > IMAGE_LIMIT {
                return Err(format!(
                    "{shown}: an image of {size} bytes is more than read takes ({IMAGE_LIMIT})"
                ));
            }
            let file = path.clone();
            // A file past the storage workers' budget reserves all of it.
            let reserved = usize::try_from(size)
                .map_or(rt::STORAGE_BUDGET, |size| size.min(rt::STORAGE_BUDGET));
            let bytes = offload(rt, Storage, reserved, move || whole(&file, size))
                .await
                .and_then(|read| read)
                .map_err(fail)?;
            let (sent, converted) = picture(rt, bytes, header, shown).await?;
            return keep(rt, cx, path, sent, converted, window != (None, None)).await;
        }
        if size > TEXT_LIMIT && window == (None, None) {
            return Err(format!(
                "{shown}: {size} bytes is more than read takes at once ({TEXT_LIMIT}); give offset and limit to read part of it"
            ));
        }
        let reserved = usize::try_from(size.min(TEXT_LIMIT)).unwrap_or(usize::MAX);
        let named = shown.clone();
        let read = offload(rt, Storage, reserved, move || {
            File::open(&path).map(|opened| lines(BufReader::new(opened), path, window, &named))
        });
        read.await.and_then(|read| read).map_err(fail)?
    }

    fn to_model(output: &Json) -> llm::Output {
        let Ok(contents) = output.parse::<Contents>() else {
            return llm::Output::Json(output.clone());
        };
        llm::Output::Text(match contents {
            Contents::Text(text) if text.from == 1 && text.lines == text.total => text.text,
            Contents::Text(Text {
                from,
                total,
                lines: 0,
                ..
            }) if from > total => format!("[the file has {total} lines; none from line {from}]"),
            Contents::Text(Text {
                from,
                total,
                lines: 0,
                ..
            }) => format!("[no lines asked for, from line {from} of {total}]"),
            Contents::Text(text) => {
                let to = text.from + text.lines - 1;
                let mut out = text.text;
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                let _ = write!(out, "[lines {}-{to} of {}]", text.from, text.total);
                out
            }
            Contents::Image(image) => {
                let Picture {
                    media_type,
                    width,
                    height,
                    bytes,
                } = &image.sent;
                let mut out = format!(
                    "{}: {media_type}, {width}x{height}, {bytes} bytes",
                    image.path.display()
                );
                if let Some(from) = &image.converted {
                    let _ = write!(
                        out,
                        " (converted from {}, {}x{}, {} bytes)",
                        from.media_type, from.width, from.height, from.bytes
                    );
                }
                if image.window_ignored {
                    out.push_str("; offset and limit are for text, and were ignored");
                }
                out
            }
        })
    }

    fn held(output: &Contents) -> Vec<Held> {
        match output {
            Contents::Image(Image {
                blob: Some(blob), ..
            }) => vec![Held::Blob(blob.clone())],
            Contents::Image(_) | Contents::Text(_) => Vec::new(),
        }
    }

    fn present(input: &ReadInput) -> Option<Card> {
        Some(Card {
            title: format!("read {}", input.path),
            ..Card::default()
        })
    }
}

/// The image `bytes` hold as it is sent, and the file as it was if what
/// is sent was converted from it.
async fn picture(
    rt: &dyn rt::Runtime,
    bytes: Vec<u8>,
    header: Header,
    shown: &str,
) -> Result<(Prepared, Option<Picture>), String> {
    let Header {
        format,
        width,
        height,
        orientation,
        decode,
    } = header;
    let fits = width.max(height) <= MAX_EDGE && bytes.len() <= MAX_BYTES;
    if SENT.contains(&format) && fits && orientation == Orientation::NoTransforms {
        let media_type = format.to_mime_type();
        let sent = Prepared {
            bytes,
            media_type,
            width,
            height,
        };
        return Ok((sent, None));
    }
    let original = Picture {
        media_type: format.to_mime_type().to_owned(),
        width,
        height,
        bytes: bytes.len() as u64,
    };
    // What converting holds at most: the file and what it decodes to,
    // then the image scaled to fit, and what that encodes to. A photo can
    // need more than the CPU workers' whole budget: it then reserves all
    // of it, and runs alone, rather than not at all.
    let decoded = usize::try_from(decode.bytes()).unwrap_or(usize::MAX);
    let work = (bytes.len())
        .saturating_add(decoded.max(SCALED))
        .saturating_add(MAX_BYTES)
        .min(rt::CPU_BUDGET);
    let converted = offload(rt, Cpu, work, move || {
        convert(bytes, format, decode, orientation)
    })
    .await
    .and_then(|converted| converted.map_err(io::Error::other))
    .map_err(|e| format!("{shown}: {e}"))?;
    Ok((converted, Some(original)))
}

/// `sent` kept as a blob the call's result holds.
async fn keep(
    rt: &dyn rt::Runtime,
    cx: &Cx,
    path: PathBuf,
    sent: Prepared,
    converted: Option<Picture>,
    window_ignored: bool,
) -> Result<Contents, String> {
    let (blobs, call) = (cx.blobs.clone(), cx.call);
    let (media_type, width, height) = (sent.media_type, sent.width, sent.height);
    let blob = offload(rt, Storage, sent.bytes.len(), move || {
        let mut writer = blobs.writer(sent.bytes.len() as u64)?;
        writer.write_all(&sent.bytes)?;
        writer.finish(media_type, BlobOrigin::ToolOutput(call))
    })
    .await
    .and_then(|kept| kept)
    .map_err(|e| format!("{}: the image could not be kept: {e}", path.display()))?;
    Ok(Contents::Image(Image {
        path,
        sent: Picture {
            media_type: media_type.to_owned(),
            width,
            height,
            bytes: blob.size,
        },
        converted,
        window_ignored,
        blob: Some(blob),
    }))
}

/// The file's size and validated image header, without loading its pixels.
fn peek(path: &Path) -> io::Result<(u64, Option<Header>)> {
    let mut reader = BufReader::new(File::open(path)?);
    let size = reader.get_ref().metadata()?.len();
    let mut head = Vec::with_capacity(HEAD);
    reader.by_ref().take(HEAD as u64).read_to_end(&mut head)?;
    reader.rewind()?;
    let header = image::guess_format(&head)
        .ok()
        .filter(|format| IMAGES.contains(format))
        .and_then(|format| Header::read(reader, format).ok());
    Ok((size, header))
}

/// The file at `path`, at most the `size` it was found to have.
fn whole(path: &Path, size: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
    File::open(path)?.take(size).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The text `reader` holds from line `offset` (from 1), at most `limit`
/// lines of it and at most [`TEXT_LIMIT`] bytes, its lines all counted.
/// Lines outside the window are counted, not read as text.
fn lines(
    mut reader: impl BufRead,
    path: PathBuf,
    (offset, limit): (Option<usize>, Option<usize>),
    shown: &str,
) -> Result<Contents, String> {
    let from = offset.unwrap_or(1).max(1);
    let to = limit.map_or(usize::MAX, |limit| from.saturating_add(limit));
    let (mut text, mut total, mut open) = (Vec::new(), 0, false);
    loop {
        if !(from..to).contains(&(total + 1)) {
            let skipped = reader
                .skip_until(b'\n')
                .map_err(|e| format!("{shown}: {e}"))?;
            if skipped == 0 {
                break;
            }
            total += 1;
            continue;
        }
        let buffer = reader.fill_buf().map_err(|e| format!("{shown}: {e}"))?;
        if buffer.is_empty() {
            break;
        }
        let end = buffer
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buffer.len(), |at| at + 1);
        if (text.len() + end) as u64 > TEXT_LIMIT {
            return Err(format!(
                "{shown}: the lines asked for are more than read takes at once ({TEXT_LIMIT} bytes); ask for fewer"
            ));
        }
        text.extend_from_slice(&buffer[..end]);
        open = buffer[end - 1] != b'\n';
        if !open {
            total += 1;
        }
        reader.consume(end);
    }
    if open {
        total += 1;
    }
    let text =
        String::from_utf8(text).map_err(|_| format!("{shown} is neither text nor an image"))?;
    let lines = to.min(total + 1).saturating_sub(from);
    Ok(Contents::Text(Text {
        path,
        text,
        from,
        lines,
        total,
    }))
}

/// What an image is sent as.
struct Prepared {
    bytes: Vec<u8>,
    media_type: &'static str,
    width: u32,
    height: u32,
}

/// `work` run on a blocking worker of `class`, reserving `bytes` while it
/// runs; `Err` when the runtime cannot admit it.
async fn offload<T: Send + 'static>(
    rt: &dyn rt::Runtime,
    class: rt::BlockingClass,
    bytes: usize,
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    let (tx, rx) = rt::oneshot::channel();
    rt.spawn_blocking(class, bytes, Box::new(move || tx.send(work())))
        .await?;
    rx.await
        .ok_or_else(|| io::Error::other("the worker panicked"))
}

/// What an image's header says: its size, how it is turned, and how it
/// decodes.
struct Header {
    format: ImageFormat,
    width: u32,
    height: u32,
    orientation: Orientation,
    decode: Decode,
}

/// How an image too large to send is decoded.
#[derive(Clone, Copy)]
enum Decode {
    /// A row at a time, each scaled as it is read, so the image is never
    /// held whole: a PNG not interlaced, as screenshots are.
    Rows,
    /// A JPEG at the smallest DCT scale at least as large as the image
    /// scaled to fit, into this many bytes.
    Scaled(u64),
    /// Whole, into this many bytes.
    Whole(u64),
}

impl Decode {
    /// The bytes decoding holds besides the image scaled to fit.
    const fn bytes(self) -> u64 {
        match self {
            Self::Rows => 0,
            Self::Scaled(bytes) | Self::Whole(bytes) => bytes,
        }
    }
}

impl Header {
    /// Validates image metadata from `reader` without decoding pixels.
    fn read(mut reader: impl BufRead + Seek, format: ImageFormat) -> Result<Self, String> {
        let orientation = match format {
            ImageFormat::Png => {
                let mut png = png::Decoder::new(&mut reader)
                    .read_info()
                    .map_err(|e| e.to_string())?;
                // eXIf may follow IDAT. Skip compressed pixels, without decoding
                // them, so pass-through never loses a trailing orientation.
                png.finish().map_err(|e| e.to_string())?;
                let info = png.info();
                let orientation = info
                    .exif_metadata
                    .as_deref()
                    .and_then(Orientation::from_exif_chunk)
                    .unwrap_or(Orientation::NoTransforms);
                if !info.interlaced {
                    return Ok(Self {
                        format,
                        width: info.width,
                        height: info.height,
                        orientation,
                        decode: Decode::Rows,
                    });
                }
                Some(orientation)
            }
            ImageFormat::Jpeg => {
                let mut jpeg = jpeg_decoder::Decoder::new(&mut reader);
                jpeg.read_info().map_err(|e| e.to_string())?;
                let info = jpeg.info().ok_or("no JPEG frame")?;
                let (width, height) = (u32::from(info.width), u32::from(info.height));
                let orientation = (jpeg.exif_data())
                    .and_then(Orientation::from_exif_chunk)
                    .unwrap_or(Orientation::NoTransforms);
                // CMYK and 16-bit JPEGs decode whole, through `image`.
                if matches!(
                    info.pixel_format,
                    jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8
                ) {
                    let (to_width, to_height) = fit(width, height);
                    let scale = |n: u32| u16::try_from(n).map_err(|e| e.to_string());
                    let (scaled_width, scaled_height) = jpeg
                        .scale(scale(to_width)?, scale(to_height)?)
                        .map_err(|e| e.to_string())?;
                    let decoded = u64::from(scaled_width)
                        * u64::from(scaled_height)
                        * info.pixel_format.pixel_bytes() as u64;
                    let decode = Decode::Scaled(decoded);
                    return Ok(Self {
                        format,
                        width,
                        height,
                        orientation,
                        decode,
                    });
                }
                None
            }
            _ => None,
        };
        reader.rewind().map_err(|e| e.to_string())?;
        let reader = ImageReader::with_format(reader, format);
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let (width, height) = decoder.dimensions();
        let orientation = orientation
            .unwrap_or_else(|| decoder.orientation().unwrap_or(Orientation::NoTransforms));
        let decode = Decode::Whole(decoder.total_bytes());
        Ok(Self {
            format,
            width,
            height,
            orientation,
            decode,
        })
    }
}

/// An image of `width` by `height` scaled to fit [`MAX_EDGE`], its aspect
/// kept; as it is if it fits.
fn fit(width: u32, height: u32) -> (u32, u32) {
    let longest = u64::from(width.max(height));
    let edge = u64::from(MAX_EDGE).min(longest);
    let scale = |n: u32| u32::try_from((u64::from(n) * edge / longest.max(1)).max(1));
    (scale(width).unwrap_or(1), scale(height).unwrap_or(1))
}

/// The image `bytes` hold, decoded, turned upright and encoded to be sent.
/// CPU-bound: run it on a worker.
fn convert(
    bytes: Vec<u8>,
    format: ImageFormat,
    decode: Decode,
    orientation: Orientation,
) -> Result<Prepared, String> {
    let mut image = match decode {
        Decode::Rows => rows(&bytes)?,
        Decode::Scaled(_) => scaled(&bytes)?,
        Decode::Whole(_) => {
            let reader = ImageReader::with_format(Cursor::new(&bytes), format);
            reader.decode().map_err(|e| e.to_string())?
        }
    };
    drop(bytes);
    image.apply_orientation(orientation);
    encode(image)
}

/// The JPEG `bytes` hold, decoded at the smallest DCT scale at least as
/// large as it scaled to fit: an eighth of the work and memory, or less.
fn scaled(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut jpeg = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    jpeg.read_info().map_err(|e| e.to_string())?;
    let info = jpeg.info().ok_or("no JPEG frame")?;
    let (to_width, to_height) = fit(u32::from(info.width), u32::from(info.height));
    let scale = |n: u32| u16::try_from(n).map_err(|e| e.to_string());
    let (width, height) = jpeg
        .scale(scale(to_width)?, scale(to_height)?)
        .map_err(|e| e.to_string())?;
    let pixels = jpeg.decode().map_err(|e| e.to_string())?;
    let (width, height) = (u32::from(width), u32::from(height));
    let image = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            image::GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            image::RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
        }
        jpeg_decoder::PixelFormat::L16 | jpeg_decoder::PixelFormat::CMYK32 => None,
    };
    image.ok_or_else(|| "the JPEG did not decode to pixels".to_owned())
}

/// The PNG `bytes` hold scaled to fit [`MAX_EDGE`] as it is decoded, a row
/// at a time: each pixel of the result the mean of the pixels it covers,
/// as [`DynamicImage::thumbnail`] makes it of a whole image.
fn rows(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (width, height) = (reader.info().width, reader.info().height);
    let channels = reader.output_color_type().0.samples();
    let (to_width, to_height) = fit(width, height);
    let (to_width, to_height) = (u64::from(to_width), u64::from(to_height));
    let index = |n: u64| usize::try_from(n).map_err(|e| e.to_string());
    let columns: Vec<usize> = (0..u64::from(width))
        .map(|x| index(x * to_width / u64::from(width)))
        .collect::<Result<_, _>>()?;
    let row_bytes = index(to_width)? * channels;
    let mut out = vec![0u8; row_bytes * index(to_height)?];
    let (mut sums, mut counts) = (vec![0u32; row_bytes], vec![0u32; index(to_width)?]);
    let mut flush = |at: usize, sums: &mut [u32], counts: &mut [u32]| {
        let row = &mut out[at * row_bytes..][..row_bytes];
        for ((value, sum), count) in row.iter_mut().zip(sums.iter()).zip(
            counts
                .iter()
                .flat_map(|count| std::iter::repeat_n(*count, channels)),
        ) {
            *value = u8::try_from((sum + count / 2) / count.max(1)).unwrap_or(u8::MAX);
        }
        sums.fill(0);
        counts.fill(0);
    };
    let (mut y, mut at) = (0, 0);
    while let Some(row) = reader.next_row().map_err(|e| e.to_string())? {
        let to = index(y * to_height / u64::from(height))?;
        if to != at {
            flush(at, &mut sums, &mut counts);
            at = to;
        }
        for (pixel, &x) in row.data().chunks_exact(channels).zip(&columns) {
            counts[x] += 1;
            for (sum, &sample) in sums[x * channels..].iter_mut().zip(pixel) {
                *sum += u32::from(sample);
            }
        }
        y += 1;
    }
    flush(at, &mut sums, &mut counts);
    let (to_width, to_height) = (
        u32::try_from(to_width).map_err(|e| e.to_string())?,
        u32::try_from(to_height).map_err(|e| e.to_string())?,
    );
    let image = match reader.output_color_type().0 {
        png::ColorType::Grayscale => {
            image::GrayImage::from_raw(to_width, to_height, out).map(DynamicImage::ImageLuma8)
        }
        png::ColorType::GrayscaleAlpha => {
            image::GrayAlphaImage::from_raw(to_width, to_height, out).map(DynamicImage::ImageLumaA8)
        }
        png::ColorType::Rgb => {
            image::RgbImage::from_raw(to_width, to_height, out).map(DynamicImage::ImageRgb8)
        }
        png::ColorType::Rgba => {
            image::RgbaImage::from_raw(to_width, to_height, out).map(DynamicImage::ImageRgba8)
        }
        png::ColorType::Indexed => None,
    };
    image.ok_or_else(|| "the PNG did not decode to pixels".to_owned())
}

/// `image` scaled to fit [`MAX_EDGE`] and encoded within [`MAX_BYTES`]:
/// as PNG, which keeps alpha, when it has any and that fits, else as JPEG,
/// a quarter smaller each time it does not fit.
fn encode(image: DynamicImage) -> Result<Prepared, String> {
    let fit = |image: DynamicImage, edge: u32| {
        if image.width().max(image.height()) <= edge {
            return image;
        }
        // Area sampling: each pixel counts once, an order of magnitude
        // faster than a filter at a quality alike for shrinking.
        image.thumbnail(edge, edge)
    };
    let image = fit(image, MAX_EDGE);
    if image.color().has_alpha() {
        let mut png = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        );
        image
            .write_with_encoder(encoder)
            .map_err(|e| e.to_string())?;
        if png.len() <= MAX_BYTES {
            let (width, height) = (image.width(), image.height());
            let media_type = ImageFormat::Png.to_mime_type();
            return Ok(Prepared {
                bytes: png,
                media_type,
                width,
                height,
            });
        }
    }
    let mut image = DynamicImage::ImageRgb8(image.to_rgb8());
    loop {
        let mut jpeg = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85);
        image
            .write_with_encoder(encoder)
            .map_err(|e| e.to_string())?;
        let (width, height) = (image.width(), image.height());
        if jpeg.len() <= MAX_BYTES {
            let media_type = ImageFormat::Jpeg.to_mime_type();
            return Ok(Prepared {
                bytes: jpeg,
                media_type,
                width,
                height,
            });
        }
        if width.max(height) < 64 {
            return Err("the image does not fit what providers take".into());
        }
        image = fit(image, width.max(height) / 4 * 3);
    }
}
