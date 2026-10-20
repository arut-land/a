//! How often the bytes of a reply are copied, counted by an allocator that
//! counts per thread, so tests running beside each other do not disturb
//! each other's counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io;
use std::path::Path;

use llm::http::{BoxFuture, Http, Payload, Response, body};
use llm::protocols::responses;
use llm::{Accumulator, AssistantPart, Ended, Event, Message, Provider, Request};

struct Counting;

thread_local! {
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

fn count(size: usize) {
    // A thread being torn down counts nothing.
    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
}

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        count(new);
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The bytes `f` allocates on this thread.
fn allocated<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = BYTES.with(Cell::get);
    let out = f();
    (out, BYTES.with(Cell::get) - before)
}

struct Recorded(Vec<u8>);

impl Http for Recorded {
    fn send(&self, _: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let chunks = self.0.chunks(1024).map(<[u8]>::to_vec);
        let response = http::Response::builder()
            .status(200)
            .body(body(chunks.collect::<Vec<_>>()))
            .expect("response");
        Box::pin(std::future::ready(Ok(response)))
    }
}

/// A recorded reply of four paragraphs, streamed in some 350 deltas.
fn recorded() -> Recorded {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rig/cassettes/openai/corpus_breadth/text_delta_stop.yaml");
    let text = std::fs::read_to_string(path).expect("cassette");
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).expect("yaml");
    let body = doc["then"]["body"].as_str().expect("a body");
    Recorded(body.as_bytes().to_vec())
}

#[test]
fn a_streamed_token_is_copied_once() {
    let (host, quirks) = responses::HOSTS[0];
    let provider = Provider::new(
        host.id,
        "http://recorded",
        llm::Auth::None,
        responses::Responses::new(quirks),
    );
    let request = Request {
        model: "gpt-4o".into(),
        messages: vec![Message::user("hi")].into(),
        ..Request::default()
    };
    let mut events = llm::stream(&recorded(), &provider, &request);
    let (mut read, mut pieces, mut ended) = (Vec::new(), String::new(), None);
    while let Some(event) = rt::block_on(events.next()) {
        let event = event.expect("an event");
        match event {
            Event::Delta { text, .. } => pieces.push_str(text),
            Event::End {
                ended: Ended::Text { text },
                ..
            } => ended = Some(text.as_ptr()),
            _ => {}
        }
        read.push(event.clone());
    }
    let reply = events.finish(None).expect("the reply");
    let [AssistantPart::Text(answer)] = reply.message.content.as_slice() else {
        panic!("one text part: {:?}", reply.message.content);
    };
    assert!(read.len() > 300 && answer.text.len() > 2000);
    assert_eq!(answer.text, pieces, "the end states what streamed");
    // The buffer the text grew in while it streamed is the reply's: moved
    // on at its end, not copied.
    assert_eq!(ended, Some(answer.text.as_ptr()));
    // Folding the events into a reply copies none of the text that
    // streamed: a piece lent to the reader is only read.
    let mut reply = Accumulator::new();
    for event in read {
        let delta = matches!(event, Event::Delta { .. });
        let (pushed, bytes) = allocated(|| reply.push(event));
        pushed.expect("in order");
        assert!(!delta || bytes == 0, "a delta folded with {bytes} bytes");
    }
}
