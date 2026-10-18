//! Language models over their wire protocols. A [`Provider`] is data (where,
//! how to authenticate, which [`Protocol`] with which host quirks); a
//! protocol is a pure encoder plus a pure frame decoder; [`stream`] joins
//! them over any [`Http`] client. Requests and replies use one
//! provider-agnostic [`spec`].

mod accumulate;
mod client;
mod error;
pub mod hosts;
pub mod http;
pub mod json;
pub mod models;
mod prepare;
pub mod protocol;
pub mod protocols;
pub mod replay;
pub mod spec;
pub mod sse;
#[cfg(feature = "ureq")]
mod ureq;
pub mod writer;

pub use accumulate::{Accumulator, merge_meta};
pub use client::{Events, generate, origin, stream};
pub use error::{Error, Kind};
pub use http::Http;
pub use prepare::{NO_RESULT, prepare};
pub use protocol::{
    Auth, Decoder, Encoded, Flow, Frame, Framing, Protocol, Provider, Secret, Target,
};
pub use spec::*;
#[cfg(feature = "ureq")]
pub use ureq::Ureq;
