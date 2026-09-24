//! Text recognition through the OCR engine that ships with Windows.
//!
//! `Windows.Media.Ocr` is part of the OS, so this costs no model download and
//! no extra binary weight — the opposite of bundling Tesseract. It supports
//! whatever languages the user has installed, which for a Chinese desktop
//! means Simplified Chinese out of the box.
//!
//! Recognition quality is dominated by what the engine is fed, so the frame is
//! contrast-stretched, converted to 8-bit grey and upscaled before it is
//! handed over. All three measurably help on screen text, which is thin and
//! low-contrast compared with the photographs the engine was tuned for.

use std::future::IntoFuture;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use windows::Foundation::Rect as WinRect;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

/// Below this width the frame is scaled up before recognition. Screen text is
/// small; the recogniser does noticeably better with more pixels per glyph.
const UPSCALE_BELOW: i32 = 1500;
const UPSCALE_FACTOR: i32 = 2;
/// Small crops get a third pass of magnification: UI text at 100% scaling is
/// only about 14 px tall, and the recogniser wants roughly three times that.
const SMALL_WIDTH: i32 = 1100;
const SMALL_FACTOR: i32 = 3;

/// One recognised word, positioned in the coordinate space of the image that
/// was passed in.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl OcrWord {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Whether `point` falls inside the word, padded slightly so the thin
    /// glyph boxes are comfortable to hit with a mouse.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x - 2.0 && x <= self.right() + 2.0 && y >= self.y - 3.0 && y <= self.bottom() + 3.0
    }
}

/// A recognised line: the engine's own text for the whole line, plus the word
/// boxes that make it selectable.
///
/// Keeping the line text matters for quality. Joining words back together with
/// guessed spaces loses punctuation and mangles mixed CJK/Latin runs, whereas
/// the engine already knows exactly what the line says.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub words: Vec<OcrWord>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl OcrLine {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x - 2.0 && x <= self.right() + 2.0 && y >= self.y - 3.0 && y <= self.bottom() + 3.0
    }

    /// Whether the line's vertical band overlaps `[y0, y1]`.
    pub fn overlaps_rows(&self, y0: f32, y1: f32) -> bool {
        !(self.bottom() < y0 || y1 < self.y)
    }

    /// The words of this line that a horizontal range covers.
    ///
    /// The range between the first and last hit is filled in, so a selection
    /// that lands in the gaps between words — which is easy to do with a mouse
    /// — still copies the whole phrase instead of dropping the word in the
    /// middle. This is the "middle of a line could not be copied" bug.
    pub fn words_in(&self, x0: f32, x1: f32) -> Vec<&OcrWord> {
        let (left, right) = (x0.min(x1), x0.max(x1));
        let hits: Vec<usize> = self
            .words
            .iter()
            .enumerate()
            .filter(|(_, word)| !(word.right() < left || right < word.x))
            .map(|(index, _)| index)
            .collect();
        let (Some(first), Some(last)) = (hits.first(), hits.last()) else {
            return Vec::new();
        };
        self.words[*first..=*last].iter().collect()
    }

    /// Joins the chosen words, inserting a space only where the gap between
    /// them is wide enough to have been one on screen.
    pub fn join(&self, words: &[&OcrWord]) -> String {
        if words.len() == self.words.len() {
            // The whole line was selected, so the engine's own text — with its
            // punctuation and spacing — is the better answer.
            return self.text.clone();
        }
        let mut text = String::new();
        let mut previous: Option<&OcrWord> = None;
        for word in words {
            if let Some(last) = previous {
                let gap = word.x - last.right();
                if gap > last.h * 0.35 && !text.ends_with(' ') && !is_cjk_edge(&text, &word.text) {
                    text.push(' ');
                }
            }
            text.push_str(&word.text);
            previous = Some(word);
        }
        text
    }
}

/// Whether a space would be wrong between these two runs (CJK text is set
/// without spaces, so inserting one would corrupt the copy).
fn is_cjk_edge(left: &str, right: &str) -> bool {
    is_cjk(left.chars().next_back()) || is_cjk(right.chars().next())
}

fn is_cjk(character: Option<char>) -> bool {
    character
        .map(|c| matches!(c as u32, 0x3000..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF))
        .unwrap_or(false)
}

/// Removes the spaces Windows OCR sprinkles between Chinese characters.
///
/// The engine segments CJK text into small runs and joins them with spaces,
/// which turns "继续驻留托盘" into "继 续 驻 留 托 盘" — technically the same
/// characters, but not something anyone wants pasted.
fn compact_cjk(text: &str) -> String {
    if !text.contains(' ') {
        return text.to_string();
    }
    let characters: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (index, character) in characters.iter().enumerate() {
        if *character == ' ' {
            let before = index.checked_sub(1).and_then(|i| characters.get(i)).copied();
            let after = characters.get(index + 1).copied();
            if is_cjk(before) && is_cjk(after) {
                continue;
            }
        }
        out.push(*character);
    }
    out
}

/// Recognises the text in a top-down BGRA frame.
///
/// Runs on its own thread: WinRT needs a COM apartment, and whichever thread
/// calls this may already belong to one initialised with a different model.
/// The call blocks that thread until the engine is done, which is why it is
/// never the UI thread.
pub fn recognize(bgra: Vec<u8>, width: i32, height: i32) -> Result<Vec<OcrLine>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("xshot-ocr".into())
        .spawn(move || {
            let _ = sender.send(recognize_blocking(&bgra, width, height));
        })
        .context("spawning the OCR thread")?;

    receiver
        .recv()
        .map_err(|_| anyhow::anyhow!("the OCR thread stopped without answering"))?
}

fn recognize_blocking(bgra: &[u8], width: i32, height: i32) -> Result<Vec<OcrLine>> {
    if width <= 0 || height <= 0 {
        bail!("cannot recognise text in an empty image");
    }
    init_apartment();

    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .context("no OCR language is installed for this user")?;
    let limit = OcrEngine::MaxImageDimension().unwrap_or(10_000) as i32;

    let factor = if width < SMALL_WIDTH && width * SMALL_FACTOR <= limit {
        SMALL_FACTOR
    } else if width < UPSCALE_BELOW && width * UPSCALE_FACTOR <= limit {
        UPSCALE_FACTOR
    } else {
        1
    };
    if width * factor > limit || height * factor > limit {
        bail!("image is larger than the OCR engine supports");
    }

    let grey = to_grey(bgra, width, height, factor);
    let (scaled_width, scaled_height) = (width * factor, height * factor);

    let buffer = CryptographicBuffer::CreateFromByteArray(&grey).context("copying pixels")?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Gray8,
        scaled_width,
        scaled_height,
    )
    .context("building a SoftwareBitmap")?;

    let result = block_on(engine.RecognizeAsync(&bitmap)?.into_future())
        .context("the OCR engine failed")?;

    let scale = factor as f32;
    let mut lines = Vec::new();
    for line in result.Lines().context("reading OCR lines")? {
        let text = compact_cjk(&line.Text().context("reading an OCR line")?.to_string());
        if text.trim().is_empty() {
            continue;
        }
        let mut words = Vec::new();
        for word in line.Words().context("reading OCR words")? {
            let word_text = word.Text().context("reading an OCR word")?.to_string();
            if word_text.trim().is_empty() {
                continue;
            }
            let rect: WinRect = word.BoundingRect().context("reading a word box")?;
            words.push(OcrWord {
                text: word_text,
                x: rect.X / scale,
                y: rect.Y / scale,
                w: rect.Width / scale,
                h: rect.Height / scale,
            });
        }
        if words.is_empty() {
            continue;
        }
        let x = words.iter().map(|w| w.x).fold(f32::MAX, f32::min);
        let y = words.iter().map(|w| w.y).fold(f32::MAX, f32::min);
        let right = words.iter().map(|w| w.right()).fold(f32::MIN, f32::max);
        let bottom = words.iter().map(|w| w.bottom()).fold(f32::MIN, f32::max);
        lines.push(OcrLine {
            text,
            words,
            x,
            y,
            w: right - x,
            h: bottom - y,
        });
    }
    Ok(lines)
}

fn init_apartment() {
    // Fails harmlessly when the thread is already in an apartment.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

/// Greyscale, contrast-stretched and optionally upscaled copy of the frame.
///
/// The stretch puts the darkest and lightest 2% of pixels at the ends of the
/// range, which turns washed-out anti-aliased screen text into something much
/// closer to the black-on-white the recogniser expects.
fn to_grey(bgra: &[u8], width: i32, height: i32, factor: i32) -> Vec<u8> {
    let (source_width, source_height) = (width as usize, height as usize);
    let mut grey = vec![0u8; source_width * source_height];
    for index in 0..source_width * source_height {
        let pixel = index * 4;
        let b = bgra[pixel] as u32;
        let g = bgra[pixel + 1] as u32;
        let r = bgra[pixel + 2] as u32;
        // Rec. 601 luma, integer arithmetic.
        grey[index] = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
    }

    let mut histogram = [0u32; 256];
    for value in &grey {
        histogram[*value as usize] += 1;
    }
    let total = grey.len() as u32;
    let mut low = 0u8;
    let mut high = 255u8;
    let mut running = 0u32;
    for (value, count) in histogram.iter().enumerate() {
        running += count;
        if running >= total / 50 {
            low = value as u8;
            break;
        }
    }
    running = 0;
    for (value, count) in histogram.iter().enumerate().rev() {
        running += count;
        if running >= total / 50 {
            high = value as u8;
            break;
        }
    }
    if high > low + 8 {
        let span = (high - low) as u32;
        for value in grey.iter_mut() {
            let stretched = (value.saturating_sub(low) as u32 * 255) / span;
            *value = stretched.min(255) as u8;
        }
    }

    if factor == 1 {
        return grey;
    }
    upscale_grey(&grey, source_width, source_height, factor as usize)
}

/// Bilinear upscale of an 8-bit grey image.
fn upscale_grey(source: &[u8], width: usize, height: usize, factor: usize) -> Vec<u8> {
    let (target_width, target_height) = (width * factor, height * factor);
    let mut out = vec![0u8; target_width * target_height];
    for target_y in 0..target_height {
        let source_y = (target_y as f32 + 0.5) / factor as f32 - 0.5;
        let y0 = source_y.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(height - 1);
        let wy = (source_y - y0 as f32).clamp(0.0, 1.0);
        for target_x in 0..target_width {
            let source_x = (target_x as f32 + 0.5) / factor as f32 - 0.5;
            let x0 = source_x.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(width - 1);
            let wx = (source_x - x0 as f32).clamp(0.0, 1.0);

            let top = source[y0 * width + x0] as f32 * (1.0 - wx)
                + source[y0 * width + x1] as f32 * wx;
            let bottom = source[y1 * width + x0] as f32 * (1.0 - wx)
                + source[y1 * width + x1] as f32 * wx;
            out[target_y * target_width + target_x] =
                (top * (1.0 - wy) + bottom * wy).round() as u8;
        }
    }
    out
}

/// Blocks until a WinRT operation finishes.
///
/// Polling with an inert waker is deliberate: the operation completes on a
/// WinRT thread-pool thread, the status flag is what actually matters, and the
/// only cost is a handful of wake-ups while the engine works.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct Inert;
    impl std::task::Wake for Inert {
        fn wake(self: std::sync::Arc<Self>) {}
    }

    let waker = Waker::from(std::sync::Arc::new(Inert));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::sleep(Duration::from_millis(4)),
        }
    }
}
