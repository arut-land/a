//! `read` on real files: text as text, and an image held as a blob the
//! result references, as its own bytes where providers read them, else
//! converted to fit.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use ext::host::Registry;
use ext::testing::Rig;
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use protocol::{Held, Outcome, Payload};
use serde_json::{Value, json};

fn registry() -> Registry {
    Registry::default()
        .with(&ext_shell::Shell::default())
        .and_then(|r| r.with(&ext_edit::Edit))
        .unwrap()
}

/// The low byte of `n`: a gradient that wraps.
fn low(n: u32) -> u8 {
    n.to_le_bytes()[0]
}

/// A gradient, so an encoder cannot shrink it to nothing.
fn picture(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| Rgb([low(x), low(y), low(x * y)]))
}

fn write(rig: &Rig, name: &str, image: &RgbImage, format: ImageFormat) -> Vec<u8> {
    let path = rig.workspace().join(name);
    image.save_with_format(&path, format).unwrap();
    std::fs::read(path).unwrap()
}

/// What the model is shown of a call's outcome.
fn shown(rig: &Rig, outcome: &protocol::ToolOutcome) -> String {
    match rig.registry.tools().to_model("read", outcome, None).0 {
        llm::Output::Text(text) => text,
        other => panic!("{other:?}"),
    }
}

/// The call's output, and the bytes of the blob it holds.
fn read(rig: &Rig, path: &str) -> (Value, Vec<u8>, protocol::BlobRef) {
    read_with_peak(rig, path, None)
}

fn read_with_peak(
    rig: &Rig,
    path: &str,
    ceiling: Option<usize>,
) -> (Value, Vec<u8>, protocol::BlobRef) {
    let input = json!({ "path": path });
    let called = within_peak(path, ceiling, || rig.called("read", &input));
    let Outcome::Done {
        output: Payload::Inline(output),
    } = called.outcome
    else {
        panic!("{path}: {:?}", called.outcome);
    };
    let [Held::Blob(blob)] = &called.held[..] else {
        panic!("{path}: holds {:?}", called.held);
    };
    let mut bytes = Vec::new();
    rig.blobs
        .open(&blob.id)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    (output.parse().unwrap(), bytes, blob.clone())
}

#[test]
fn an_image_providers_read_is_sent_as_its_own_bytes() {
    let rig = Rig::new("read-image", registry());
    for (name, format, media_type) in [
        ("shot.png", ImageFormat::Png, "image/png"),
        ("photo.jpg", ImageFormat::Jpeg, "image/jpeg"),
    ] {
        let file = write(&rig, name, &picture(40, 30), format);
        let (output, bytes, blob) = read(&rig, name);
        assert_eq!(bytes, file, "{name}: the file as it is");
        assert_eq!(blob.media_type, media_type);
        let image = &output["image"];
        assert_eq!(
            image["sent"],
            json!({"media_type": media_type, "width": 40, "height": 30, "bytes": file.len()})
        );
        assert!(image.get("converted").is_none(), "{output}");
        assert!(
            !output.to_string().contains(&blob.id.to_string()),
            "the log holds the reference once, as the result's"
        );
    }
}

#[test]
fn an_image_too_large_or_unread_is_converted_to_fit() {
    let rig = Rig::new("read-convert", registry());
    let bmp = write(&rig, "old.bmp", &picture(300, 200), ImageFormat::Bmp);
    let (output, bytes, blob) = read(&rig, "old.bmp");
    assert_eq!(blob.media_type, "image/jpeg", "no alpha: JPEG");
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        ImageFormat::Jpeg,
        "what it says it is"
    );
    assert_eq!(
        output["image"]["converted"],
        json!({"media_type": "image/bmp", "width": 300, "height": 200, "bytes": bmp.len()})
    );
    assert_eq!(output["image"]["sent"]["width"], 300);

    let wide = RgbaImage::from_fn(3000, 600, |x, _| Rgba([low(x), 0, 0, 128]));
    wide.save_with_format(rig.workspace().join("wide.png"), ImageFormat::Png)
        .unwrap();
    let (output, bytes, blob) = read(&rig, "wide.png");
    assert_eq!(blob.media_type, "image/png", "alpha kept");
    let sent = image::load_from_memory(&bytes).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 400));
    assert!(sent.color().has_alpha());
    assert_eq!(output["image"]["sent"]["width"], 2000);
    assert_eq!(output["image"]["converted"]["width"], 3000);
}

#[test]
fn png_scaling_preserves_pixels_and_alpha() {
    scaled_png(2400, 1200, (2000, 1000), None);
}

#[test]
#[ignore = "resource test: run alone; converts a 59 MB decoded screenshot"]
fn a_screen_sized_png_preserves_pixels_and_alpha() {
    let _resource = RESOURCE.lock().unwrap();
    // Below the 59 MB required by a whole RGBA decode, above the scaled output.
    scaled_png(5120, 2880, (2000, 1125), Some(32 << 20));
}

fn scaled_png(width: u32, height: u32, expected: (u32, u32), ceiling: Option<usize>) {
    let rig = Rig::new("read-screen", registry());
    {
        let screen = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                low(x / 64),
                low(y / 64),
                40,
                if x < width / 2 { 255 } else { 0 },
            ])
        });
        screen
            .save_with_format(rig.workspace().join("screen.png"), ImageFormat::Png)
            .unwrap();
    }
    let (output, bytes, _) = read_with_peak(&rig, "screen.png", ceiling);
    let sent = image::load_from_memory(&bytes).unwrap().into_rgba8();
    assert_eq!(sent.dimensions(), expected);
    assert_eq!(output["image"]["converted"]["width"], width);
    assert_eq!(sent.get_pixel(10, 10).0, [0, 0, 40, 255]);
    assert_eq!(sent.get_pixel(expected.0 - 10, 10).0[3], 0);
}

#[test]
fn text_is_read_as_text_a_window_of_lines_if_asked() {
    let rig = Rig::new("read-text", registry());
    std::fs::write(rig.workspace().join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    let whole = rig.call("read", &json!({ "path": "notes.txt" }));
    let shown = |outcome: &protocol::ToolOutcome| match outcome {
        Outcome::Done {
            output: Payload::Inline(output),
        } => match rig.registry.tools().to_model("read", outcome, None).0 {
            llm::Output::Text(text) => text,
            other => panic!("{output}: {other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(shown(&whole), "one\ntwo\nthree\n");
    let part = rig.call(
        "read",
        &json!({ "path": "notes.txt", "offset": 2, "limit": 1 }),
    );
    assert_eq!(shown(&part), "two\n[lines 2-2 of 3]");
    let none = rig.call(
        "read",
        &json!({ "path": "notes.txt", "offset": 2, "limit": 0 }),
    );
    assert_eq!(shown(&none), "[no lines asked for, from line 2 of 3]");
    let beyond = rig.call("read", &json!({ "path": "notes.txt", "offset": 9 }));
    assert_eq!(shown(&beyond), "[the file has 3 lines; none from line 9]");

    std::fs::write(rig.workspace().join("b.bin"), [0, 0xff, 0xfe]).unwrap();
    let Outcome::Failed { error } = rig.call("read", &json!({ "path": "b.bin" })) else {
        panic!("binary is refused");
    };
    assert_eq!(error, "b.bin is neither text nor an image");
}

/// A note that starts as an image format's magic does (`BM`, `P1`) is
/// read as the text it is: only a header that reads as an image of a
/// format read here makes a file one.
#[test]
fn text_that_starts_like_an_image_is_read_as_text() {
    let rig = Rig::new("read-magic", registry());
    for text in ["BMW notes\n", "P1 fix login\n"] {
        std::fs::write(rig.workspace().join("note.txt"), text).unwrap();
        let outcome = rig.call("read", &json!({ "path": "note.txt" }));
        assert_eq!(shown(&rig, &outcome), text);
    }
}

#[test]
fn a_gif_is_sent_as_its_first_frame_in_png() {
    let rig = Rig::new("read-gif", registry());
    write(&rig, "a.gif", &picture(40, 30), ImageFormat::Gif);
    let (output, bytes, blob) = read(&rig, "a.gif");
    assert_eq!(blob.media_type, "image/png");
    assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Png);
    assert_eq!(output["image"]["converted"]["media_type"], "image/gif");
}

/// A photo's EXIF orientation says how it is shown; providers read no
/// metadata, so a turned one is sent turned upright.
#[test]
fn a_turned_jpeg_is_sent_upright() {
    use image::ImageEncoder as _;
    let rig = Rig::new("read-turned", registry());
    // Big-endian TIFF, one IFD entry: orientation (0x0112), short, 6 (a
    // quarter turn clockwise).
    let exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
    let mut jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg);
    encoder.set_exif_metadata(exif).unwrap();
    let image = picture(40, 30);
    encoder
        .write_image(image.as_raw(), 40, 30, image::ExtendedColorType::Rgb8)
        .unwrap();
    std::fs::write(rig.workspace().join("turned.jpg"), &jpeg).unwrap();
    let (output, bytes, _) = read(&rig, "turned.jpg");
    let sent = image::load_from_memory(&bytes).unwrap();
    assert_eq!((sent.width(), sent.height()), (30, 40));
    assert_eq!(output["image"]["converted"]["width"], 40);
}

#[test]
fn jpeg_scaling_preserves_the_aspect_ratio() {
    scaled_jpeg(2400, 1600, (2000, 1333), None);
}

#[test]
#[ignore = "resource test: run alone; converts a 24 MP photo"]
fn a_large_photo_is_converted() {
    let _resource = RESOURCE.lock().unwrap();
    // RGB needs 72 MB decoded whole; the DCT-scaled decode fits below 64 MiB.
    scaled_jpeg(6000, 4000, (2000, 1333), Some(64 << 20));
}

fn scaled_jpeg(width: u32, height: u32, expected: (u32, u32), ceiling: Option<usize>) {
    let rig = Rig::new("read-photo", registry());
    {
        let photo = picture(width, height);
        photo
            .save_with_format(rig.workspace().join("photo.jpg"), ImageFormat::Jpeg)
            .unwrap();
    }
    let (output, bytes, blob) = read_with_peak(&rig, "photo.jpg", ceiling);
    assert_eq!(blob.media_type, "image/jpeg");
    let sent = image::load_from_memory(&bytes).unwrap();
    assert_eq!((sent.width(), sent.height()), expected);
    assert_eq!(output["image"]["converted"]["width"], width);
}

#[test]
fn an_image_says_offset_and_limit_were_ignored() {
    let rig = Rig::new("read-ignored", registry());
    write(&rig, "shot.png", &picture(40, 30), ImageFormat::Png);
    let outcome = rig.call("read", &json!({ "path": "shot.png", "offset": 3 }));
    assert!(
        shown(&rig, &outcome).ends_with("; offset and limit are for text, and were ignored"),
        "{}",
        shown(&rig, &outcome)
    );
}

/// Text past what is read at once is read a window at a time, never
/// whole: the call says to give one, and one given is read.
#[test]
fn a_large_text_file_is_read_by_its_window() {
    let rig = Rig::new("read-large", registry());
    let line = format!("{}\n", "x".repeat(99));
    let lines = (9 << 20) / line.len() + 1;
    std::fs::write(rig.workspace().join("big.log"), line.repeat(lines)).unwrap();
    let Outcome::Failed { error } = rig.call("read", &json!({ "path": "big.log" })) else {
        panic!("a whole read is refused");
    };
    assert!(error.contains("give offset and limit"), "{error}");
    let outcome = rig.call(
        "read",
        &json!({ "path": "big.log", "offset": 10, "limit": 2 }),
    );
    assert_eq!(
        shown(&rig, &outcome),
        format!("{line}{line}[lines 10-11 of {lines}]")
    );
}

#[test]
fn png_orientation_is_applied_before_pass_through_or_scaling() {
    let rig = Rig::new("read-png-orientation", registry());
    let exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
    for (width, height, expected) in [(40, 30, (30, 40)), (2400, 600, (500, 2000))] {
        for trailing in [false, true] {
            let image = RgbaImage::from_fn(width, height, |x, y| {
                Rgba([
                    if x < width / 2 { 255 } else { 0 },
                    if y < height / 2 { 255 } else { 0 },
                    0,
                    255,
                ])
            });
            let file = std::fs::File::create(rig.workspace().join("turned.png")).unwrap();
            let mut encoder = png::Encoder::new(file, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            if !trailing {
                writer.write_chunk(png::chunk::eXIf, exif).unwrap();
            }
            writer.write_image_data(image.as_raw()).unwrap();
            if trailing {
                writer.write_chunk(png::chunk::eXIf, exif).unwrap();
            }
            writer.finish().unwrap();
            let (output, bytes, blob) = read(&rig, "turned.png");
            assert_eq!(blob.media_type, "image/png");
            assert_eq!(output["image"]["converted"]["width"], width);
            let sent = image::load_from_memory(&bytes).unwrap().into_rgba8();
            assert_eq!(
                sent.dimensions(),
                expected,
                "{width}x{height}, trailing={trailing}"
            );
            assert_eq!(sent.get_pixel(0, 0).0, [255, 0, 0, 255]);
            assert_eq!(sent.get_pixel(expected.0 - 1, 0).0, [255, 255, 0, 255]);
        }
    }
}

#[test]
fn unselected_lines_can_exceed_the_text_limit() {
    unselected_line((8 << 20) + 1, None);
}

#[test]
#[ignore = "resource test: run alone; scans a 2 GiB sparse line"]
fn unselected_lines_can_exceed_the_image_and_text_limits() {
    let _resource = RESOURCE.lock().unwrap();
    unselected_line((2 << 30) + 1, Some(8 << 20));
}

fn unselected_line(length: u64, ceiling: Option<usize>) {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let rig = Rig::new("read-long-line", registry());
    let mut file = std::fs::File::create(rig.workspace().join("long.txt")).unwrap();
    file.write_all(b"before\n").unwrap();
    file.seek(SeekFrom::Start(length)).unwrap();
    file.write_all(b"\nafter\n").unwrap();
    drop(file);
    let input = json!({"path": "long.txt", "offset": 3, "limit": 1});
    let outcome = within_peak("sparse text window", ceiling, || rig.call("read", &input));
    assert_eq!(shown(&rig, &outcome), "after\n[lines 3-3 of 3]");
}

#[test]
fn an_image_magic_prefix_still_allows_a_large_text_window() {
    image_magic_text((8 << 20) + 1, None);
}

#[test]
#[ignore = "resource test: run alone; crosses the 512 MiB image limit"]
fn an_image_magic_prefix_does_not_limit_a_text_files_size() {
    let _resource = RESOURCE.lock().unwrap();
    image_magic_text((512 << 20) + 1, Some(8 << 20));
}

fn image_magic_text(length: u64, ceiling: Option<usize>) {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let rig = Rig::new("read-large-magic", registry());
    let mut file = std::fs::File::create(rig.workspace().join("notes.txt")).unwrap();
    file.write_all(b"BMW notes\n").unwrap();
    file.seek(SeekFrom::Start(length)).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let input = json!({"path": "notes.txt", "limit": 1});
    let outcome = within_peak("magic text window", ceiling, || rig.call("read", &input));
    assert_eq!(shown(&rig, &outcome), "BMW notes\n[lines 1-1 of 2]");
    let Outcome::Failed { error } = rig.call("read", &json!({"path": "notes.txt"})) else {
        panic!("a large text file needs a window");
    };
    assert!(error.contains("give offset and limit"), "{error}");
}

#[test]
fn only_selected_text_is_bounded_and_utf8_can_cross_buffers() {
    let rig = Rig::new("read-selected-bound", registry());
    let selected = format!("{}🌍\n", "x".repeat(8191));
    std::fs::write(rig.workspace().join("text.txt"), &selected).unwrap();
    let outcome = rig.call("read", &json!({"path": "text.txt", "limit": 1}));
    assert_eq!(shown(&rig, &outcome), selected);
    std::fs::write(rig.workspace().join("text.txt"), "x".repeat((8 << 20) + 1)).unwrap();
    let Outcome::Failed { error } = rig.call("read", &json!({"path": "text.txt", "limit": 1}))
    else {
        panic!("the selected text is bounded");
    };
    assert!(error.contains("the lines asked for"), "{error}");
}

#[test]
#[ignore = "resource test: run alone; measures streaming over a 128 MiB file"]
fn large_file_reads_and_edits_keep_heap_independent_of_file_size() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let _resource = RESOURCE.lock().unwrap();
    let rig = Rig::new("edit-streaming-memory", registry());
    let path = rig.workspace().join("large.txt");
    let mut file = std::fs::File::create(&path).unwrap();
    file.set_len(128 << 20).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"\ntarget\n").unwrap();
    drop(file);

    let input = json!({"path": "large.txt", "offset": 2, "limit": 1});
    let outcome = within_peak("streaming read", Some(8 << 20), || rig.call("read", &input));
    assert_eq!(shown(&rig, &outcome), "target\n[lines 2-2 of 2]");

    let input = json!({"path": "large.txt", "edits": [{"old": "target", "new": "newest"}]});
    let outcome = within_peak("streaming edit", Some(8 << 20), || rig.call("edit", &input));
    let Outcome::Done {
        output: Payload::Inline(output),
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    let output: Value = output.parse().unwrap();
    assert_eq!(output["publication"]["visible"], true);
    assert_eq!(output["publication"]["durability"], "not_requested");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), (128 << 20) + 8);
    let mut file = std::fs::File::open(path).unwrap();
    file.seek(SeekFrom::End(-8)).unwrap();
    let mut tail = String::new();
    file.read_to_string(&mut tail).unwrap();
    assert_eq!(tail, "\nnewest\n");
}

#[test]
#[ignore = "resource test: run alone; checks the allocation measurement phases"]
fn allocation_phases_exclude_fixtures_and_reset_their_peak() {
    let _resource = RESOURCE.lock().unwrap();
    let fixture = vec![0u8; 1 << 20];
    std::hint::black_box(&fixture);
    drop(fixture);
    let (bytes, first) = allocation_peak(|| {
        let mut bytes = Vec::with_capacity(4 << 10);
        bytes.resize(64 << 10, 0u8);
        bytes
    });
    assert!(
        (bytes.len()..128 << 10).contains(&first),
        "first peak: {first}"
    );
    drop(bytes);
    let (bytes, second) = allocation_peak(|| vec![0u8; 8 << 10]);
    assert!(
        (bytes.len()..16 << 10).contains(&second),
        "second peak: {second}"
    );
}

// Resource tests run alone: the counter includes all of the runtime's workers.
#[test]
#[ignore = "resource test: run alone; converts eight concurrent 12 MP images"]
fn waiting_image_reads_do_not_retain_whole_inputs() {
    let _resource = RESOURCE.lock().unwrap();
    let rig = Rig::new("read-admission", registry());
    write(&rig, "image.bmp", &picture(4000, 3000), ImageFormat::Bmp);
    let together = std::sync::Barrier::new(8);
    let called = within_peak("concurrent images", Some(128 << 20), || {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        together.wait();
                        rig.called("read", &json!({"path": "image.bmp"}))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        })
    });
    for call in called {
        assert!(matches!(call.outcome, Outcome::Done { .. }), "{call:?}");
        let [Held::Blob(blob)] = &call.held[..] else {
            panic!("the image result must hold its blob: {:?}", call.held);
        };
        let mut bytes = Vec::new();
        rig.blobs
            .open(&blob.id)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let sent = image::load_from_memory(&bytes).unwrap();
        assert_eq!((sent.width(), sent.height()), (2000, 1500));
    }
}

#[test]
#[ignore = "resource test: run alone; reads an image larger than both worker budgets"]
fn images_larger_than_the_retained_budget_still_complete() {
    let _resource = RESOURCE.lock().unwrap();
    let rig = Rig::new("read-oversized", registry());
    write(&rig, "large.bmp", &picture(5000, 5000), ImageFormat::Bmp);
    assert!(
        std::fs::metadata(rig.workspace().join("large.bmp"))
            .unwrap()
            .len()
            > rt::CPU_BUDGET as u64
    );
    let (_, bytes, _) = read(&rig, "large.bmp");
    let sent = image::load_from_memory(&bytes).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 2000));
}

static RESOURCE: Mutex<()> = Mutex::new(());
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// This test binary alone counts requested live heap; production keeps System.
#[allow(unsafe_code)]
// SAFETY: allocation contracts, pointers and layouts pass unchanged to System.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller supplies System's live pointer and its layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller supplies System's live pointer, layout and a nonzero size.
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            let live = if size >= layout.size() {
                LIVE.fetch_add(size - layout.size(), Relaxed) + size - layout.size()
            } else {
                LIVE.fetch_sub(layout.size() - size, Relaxed) - (layout.size() - size)
            };
            PEAK.fetch_max(live, Relaxed);
        }
        ptr
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocation_peak<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let result = work();
    (result, PEAK.load(Relaxed).saturating_sub(baseline))
}

fn within_peak<T>(label: &str, ceiling: Option<usize>, work: impl FnOnce() -> T) -> T {
    let Some(ceiling) = ceiling else {
        return work();
    };
    let (result, peak) = allocation_peak(work);
    eprintln!("{label}: incremental live heap peak {peak} bytes (ceiling {ceiling})");
    assert!(
        peak < ceiling,
        "{label}: peak {peak} bytes exceeds {ceiling}"
    );
    result
}
