//! `read` on real files: text as text, and an image held as a blob the
//! result references, as its own bytes where providers read them, else
//! converted to fit.

use std::io::Read as _;

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
    let called = rig.called("read", &json!({ "path": path }));
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

/// A screenshot of a 5K display decodes to 59 MB of RGBA: its rows are
/// scaled as they are read, so it is never held whole.
#[test]
fn a_screen_sized_png_is_scaled_without_being_decoded_whole() {
    let rig = Rig::new("read-screen", registry());
    let screen = RgbaImage::from_fn(5120, 2880, |x, y| {
        Rgba([low(x / 64), low(y / 64), 40, if x < 2560 { 255 } else { 0 }])
    });
    screen
        .save_with_format(rig.workspace().join("screen.png"), ImageFormat::Png)
        .unwrap();
    let (output, bytes, _) = read(&rig, "screen.png");
    let sent = image::load_from_memory(&bytes).unwrap().into_rgba8();
    assert_eq!(sent.dimensions(), (2000, 1125));
    assert_eq!(output["image"]["converted"]["width"], 5120);
    // Each pixel the mean of the block it covers.
    assert_eq!(sent.get_pixel(10, 10).0, [0, 0, 40, 255]);
    assert_eq!(sent.get_pixel(1990, 10).0[3], 0);
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

/// A 24 MP photo decodes to more than the CPU workers' whole budget, were
/// it decoded whole: it is decoded at a DCT scale, and converted still.
#[test]
fn a_large_photo_is_converted() {
    let rig = Rig::new("read-photo", registry());
    let photo = image::GrayImage::from_fn(6000, 4000, |x, y| image::Luma([low(x ^ y)]));
    photo
        .save_with_format(rig.workspace().join("photo.jpg"), ImageFormat::Jpeg)
        .unwrap();
    let (output, bytes, blob) = read(&rig, "photo.jpg");
    assert_eq!(blob.media_type, "image/jpeg");
    let sent = image::load_from_memory(&bytes).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 1333));
    assert_eq!(output["image"]["converted"]["width"], 6000);
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
fn unselected_lines_can_exceed_the_image_and_text_limits() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let rig = Rig::new("read-long-line", registry());
    let mut file = std::fs::File::create(rig.workspace().join("long.txt")).unwrap();
    file.write_all(b"before\n").unwrap();
    file.seek(SeekFrom::Start((2 << 30) + 1)).unwrap();
    file.write_all(b"\nafter\n").unwrap();
    drop(file);
    let outcome = rig.call(
        "read",
        &json!({"path": "long.txt", "offset": 3, "limit": 1}),
    );
    assert_eq!(shown(&rig, &outcome), "after\n[lines 3-3 of 3]");
}

#[test]
fn an_image_magic_prefix_does_not_limit_a_text_files_size() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let rig = Rig::new("read-large-magic", registry());
    let mut file = std::fs::File::create(rig.workspace().join("notes.txt")).unwrap();
    file.write_all(b"BMW notes\n").unwrap();
    file.seek(SeekFrom::Start((512 << 20) + 1)).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let outcome = rig.call("read", &json!({"path": "notes.txt", "limit": 1}));
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
