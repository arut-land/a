//! Language models over their wire protocols. A [`Provider`] is data (where,
//! how to authenticate, which [`Protocol`] with which host quirks); a
//! protocol is a pure encoder plus a pure frame decoder; [`stream`] joins
//! them over any [`Http`] client. Requests and replies use one
//! provider-agnostic [`spec`].

mod accumulate;
mod client;
mod conversation;
mod error;
pub mod hosts;
pub use transport as http;
mod json;
pub mod models;
mod prepare;
pub mod protocol;
pub mod protocols;
mod replay;
pub mod spec;
mod sse;
mod writer;

pub use accumulate::Accumulator;
pub use client::{Events, generate, origin, stream};
pub use conversation::History;
pub use error::{Error, Kind};
pub use http::Http;
pub use json::parse_partial_arguments;
pub use prepare::NO_RESULT;
pub use protocol::{Auth, Decoder, Encoded, Flow, Frame, Protocol, Provider, Secret, Target};
pub use spec::*;
#[cfg(feature = "ureq")]
pub use transport::{RedirectPolicy as Redirects, Ureq};
