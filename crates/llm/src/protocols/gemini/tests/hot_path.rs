//! What decoding recorded streams allocates, counted on this thread. A
//! chunk that continues a text run parses into types that borrow its text,
//! so it allocates little beyond what its events own; parsing it as a
//! `Value`, as the decoder once did first, allocates several times more.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use super::*;
use crate::protocol::Decoder;

/// The system allocator, counting this thread's allocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The allocations `run` makes on this thread.
fn allocations(run: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    run();
    ALLOCATIONS.with(Cell::get) - before
}

/// Every recorded Gemini stream, as its frames' data.
fn streams() -> Vec<Vec<String>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rig");
    let mut dirs = vec![root.join("cassettes/gemini")];
    let mut streams = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).expect("fixtures") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("cassette");
            for doc in serde_yaml_ng::Deserializer::from_str(&text) {
                let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).expect("yaml");
                let body = doc.get("then").and_then(|then| then.get("body"));
                let body = body
                    .and_then(serde_yaml_ng::Value::as_str)
                    .unwrap_or_default();
                let data = body.lines().filter_map(|line| line.strip_prefix("data: "));
                streams.push(data.map(str::to_owned).collect::<Vec<_>>());
            }
        }
    }
    streams.retain(|stream| !stream.is_empty());
    streams
}

/// Decode every stream, calling `each` with each chunk after a stream's
/// first and the allocations its frame made.
fn decode(streams: &[Vec<String>], mut each: impl FnMut(&str, u64)) {
    let mut events = Vec::with_capacity(64);
    for stream in streams {
        let mut decoder = super::super::decode::GeminiDecoder::new();
        for (index, data) in stream.iter().enumerate() {
            let mut flow = Ok(Flow::More);
            let frame = Frame { event: "", data };
            let made = allocations(|| flow = decoder.frame(frame, &mut events));
            if index > 0 {
                each(data, made);
            }
            events.clear();
            if !matches!(flow, Ok(Flow::More)) {
                break;
            }
        }
        let _ = decoder.end(&mut events);
        events.clear();
    }
}

#[test]
fn a_chunk_that_continues_text_allocates_a_fraction_of_its_value() {
    let streams = streams();
    let (mut chunks, mut decoded, mut parsed) = (0, 0, 0);
    decode(&streams, |data, made| {
        if data.contains("\"text\"") && !data.contains("functionCall") {
            chunks += 1;
            decoded += made;
            parsed += allocations(|| drop(serde_json::from_str::<Value>(data)));
        }
    });
    let start = Instant::now();
    for _ in 0..100 {
        decode(&streams, |_, _| {});
    }
    eprintln!(
        "{chunks} later text chunks: decoding allocates {decoded} times, parsing them as \
         values alone {parsed}; all {} recorded streams decode in {:?}",
        streams.len(),
        start.elapsed() / 100
    );
    assert!(chunks >= 20, "{chunks} text chunks");
    assert!(
        decoded * 3 < parsed,
        "{decoded} allocations against {parsed}"
    );
}
