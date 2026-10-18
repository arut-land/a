//! One module per wire protocol: its encoder, its decoder, and the hosts
//! that speak it with their quirks.

pub mod chat;
pub mod gemini;
pub mod messages;
pub mod responses;
pub(crate) mod schema;
