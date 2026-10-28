//! What a request may carry of its images. Every turn sends the whole
//! conversation again, images included, and each provider refuses a request
//! past its documented size or image count. A request past either sends the
//! newest images that fit, as a client of that provider would, and leaves
//! each older one out with a note, its other parts as they were. Nothing is
//! dropped from the conversation itself: only this request leaves them out.

use std::borrow::Cow;

use crate::error::Error;
use crate::http::Payload;
use crate::protocol::Encoded;
use crate::spec::{Message, Output, ProtocolKey, Request, Text, UserPart};

/// What replaces an image a request leaves out.
pub const IMAGE_LEFT_OUT: &str =
    "(image left out: the request carries the newest images its provider takes)";

/// What one request may carry of its images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The complete encoded request body's byte cap.
    pub bytes: u64,
    pub images: usize,
}

impl Limits {
    const fn of(request_bytes: u64, images: usize) -> Self {
        Self {
            bytes: request_bytes,
            images,
        }
    }

    /// The limits of `protocol`'s provider for `model`.
    pub fn of_protocol(protocol: ProtocolKey, model: &str) -> Self {
        match protocol {
            // 32 MB a request; 100 images for a model with a 200k window,
            // 600 for another (platform.claude.com, vision and API overview).
            ProtocolKey::AnthropicMessages => {
                let small = crate::models::context_window(model).is_none_or(|w| w <= 200_000);
                Self::of(32_000_000, if small { 100 } else { 600 })
            }
            // 20 MB a request with inline data (ai.google.dev, image
            // understanding); 3,600 images.
            ProtocolKey::GoogleGemini => Self::of(20_000_000, 3_600),
            // 512 MB a request and 1,500 images (developers.openai.com,
            // images and vision; docs/llm.md links the requirements).
            ProtocolKey::OpenaiChat | ProtocolKey::OpenaiResponses => Self::of(512_000_000, 1_500),
        }
    }
}

/// Encodes `request` within `limits`, leaving its oldest images out until
/// its actual body fits. Text and other non-image content are never cut;
/// a body still too large without images is left to the provider to reject.
pub fn fit(
    request: &Request,
    limits: Limits,
    mut encode: impl FnMut(&Request) -> Result<Encoded, Error>,
) -> Result<(Encoded, Payload), Error> {
    let mut remaining = request.messages.images();
    let mut fitted = Cow::Borrowed(request);
    let mut at = 0;
    loop {
        if remaining <= limits.images {
            let encoded = encode(&fitted)?;
            let payload = encoded.payload();
            if payload.len() as u64 <= limits.bytes || remaining == 0 {
                return Ok((encoded, payload));
            }
        }
        let messages = fitted.to_mut().messages.canonical_mut()?;
        while images(&messages[at]) == 0 {
            at += 1;
        }
        let remove = remaining
            .saturating_sub(limits.images)
            .max(1)
            .min(images(&messages[at]));
        let message = messages
            .get_mut(at)
            .expect("the oldest remaining image has a message");
        leave_out(message, remove - 1);
        remaining -= remove;
    }
}

/// The number of images in `message`.
fn images(message: &Message) -> usize {
    let image = |part: &&UserPart| matches!(part, UserPart::Media(media) if media.media_type.starts_with("image/"));
    match message {
        Message::User(user) => user.content.iter().filter(image).count(),
        Message::Tool(tool) => tool
            .results
            .iter()
            .map(|result| match &result.output {
                Output::Content(parts) => parts.iter().filter(image).count(),
                Output::Text(_) | Output::Json(_) => 0,
            })
            .sum(),
        Message::System(_) | Message::Assistant(_) => 0,
    }
}

/// Replaces images through `through`, counting from zero in message order.
fn leave_out(message: &mut Message, through: usize) {
    let mut seen = 0;
    let mut out = |part: &mut UserPart| {
        let UserPart::Media(media) = part else {
            return;
        };
        if !media.media_type.starts_with("image/") {
            return;
        }
        if seen <= through {
            *part = UserPart::Text(Text::new(IMAGE_LEFT_OUT));
        }
        seen += 1;
    };
    match message {
        Message::User(user) => user.content.iter_mut().for_each(&mut out),
        Message::Tool(tool) => {
            for result in &mut tool.results {
                if let Output::Content(parts) = &mut result.output {
                    parts.iter_mut().for_each(&mut out);
                }
            }
        }
        Message::System(_) | Message::Assistant(_) => {}
    }
}
