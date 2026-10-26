//! Content codings over the body stream, preserving partial SSE reads.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use async_compression::futures::bufread::{GzipDecoder, GzipEncoder};
use futures_lite::io::{AsyncBufRead, AsyncRead, AsyncReadExt as _, BufReader};
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH};

use super::CHUNK;
use crate::{Body, Compression, Payload, Response};

type Read<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;

pub(super) fn decode(response: Response) -> io::Result<Response> {
    let (mut head, body) = response.into_parts();
    let codings = head
        .headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        })
        .collect::<io::Result<Vec<_>>>()?
        .join(",")
        .to_ascii_lowercase();
    if codings.is_empty() {
        return Ok(http::Response::from_parts(head, body));
    }
    let mut reader: Read<'static> = Box::pin(Chunks {
        body,
        chunk: Vec::new(),
        at: 0,
    });
    for coding in codings.split(',').rev().map(str::trim) {
        let input = BufReader::with_capacity(CHUNK, reader);
        reader = match coding {
            "gzip" => {
                let mut decoder = GzipDecoder::new(input);
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "zstd" => Box::pin(Zstd {
                input,
                session: mzstd::Decompressor::default().into_session(),
                status: mzstd::DecoderStatus::NeedsInput,
            }),
            "identity" => Box::pin(input),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported content encoding: {coding}"),
                ));
            }
        };
    }
    head.headers.remove(CONTENT_ENCODING);
    head.headers.remove(CONTENT_LENGTH);
    Ok(http::Response::from_parts(head, stream(reader)))
}

/// Compress pieces without first concatenating the conversation. Encoded
/// pieces remain replayable and have a known length for HTTP/1.1.
pub(super) async fn encode(payload: &Payload, coding: Compression) -> io::Result<Payload> {
    let input = BufReader::with_capacity(CHUNK, Memory(payload.reader()));
    let mut zstd = (coding == Compression::Zstd)
        .then(|| mzstd::Compressor::new(mzstd::EncoderConfig::default()))
        .transpose()
        .map_err(io::Error::other)?;
    let mut encoded: Read<'_> = match coding {
        Compression::Gzip => Box::pin(GzipEncoder::new(input)),
        Compression::Zstd => Box::pin(Memory(
            zstd.as_mut()
                .expect("zstd coding builds its compressor")
                .reader(payload.reader(), mzstd::StreamConfig::default()),
        )),
    };
    let mut pieces: Vec<Arc<[u8]>> = Vec::new();
    loop {
        let mut chunk = vec![0; CHUNK];
        let n = encoded.read(&mut chunk).await?;
        if n == 0 {
            return Ok(pieces.into());
        }
        chunk.truncate(n);
        pieces.push(chunk.into());
        rt::yield_now().await;
    }
}

fn stream(reader: Read<'static>) -> Body {
    Box::pin(futures_lite::stream::unfold(
        Some(reader),
        |reader| async move {
            let mut reader = reader?;
            let mut chunk = vec![0; CHUNK];
            match reader.read(&mut chunk).await {
                Ok(0) => None,
                Ok(n) => {
                    chunk.truncate(n);
                    Some((Ok(chunk), Some(reader)))
                }
                Err(error) => Some((Err(error), None)),
            }
        },
    ))
}

struct Chunks {
    body: Body,
    chunk: Vec<u8>,
    at: usize,
}

impl AsyncBufRead for Chunks {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        while this.at == this.chunk.len() {
            match ready!(this.body.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) => {
                    this.chunk = chunk;
                    this.at = 0;
                }
                Some(Err(error)) => return Poll::Ready(Err(error)),
                None => return Poll::Ready(Ok(&[])),
            }
        }
        Poll::Ready(Ok(&this.chunk[this.at..]))
    }

    fn consume(mut self: Pin<&mut Self>, amount: usize) {
        self.at += amount;
    }
}

impl AsyncRead for Chunks {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let input = ready!(self.as_mut().poll_fill_buf(cx))?;
        let n = input.len().min(buf.len());
        buf[..n].copy_from_slice(&input[..n]);
        self.consume(n);
        Poll::Ready(Ok(n))
    }
}

/// Only instantiated with the payload's in-memory reader.
struct Memory<R>(R);

impl<R: io::Read + Unpin> AsyncRead for Memory<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.0.read(buf))
    }
}

struct Zstd {
    input: BufReader<Read<'static>>,
    session: mzstd::DecoderSessionOwned,
    status: mzstd::DecoderStatus,
}

impl AsyncRead for Zstd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        use mzstd::{DecodeOperation, DecoderStatus};
        if output.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let Self {
            input,
            session,
            status,
        } = self.get_mut();
        loop {
            if *status == DecoderStatus::Finished {
                return Poll::Ready(Ok(0));
            }
            let (source, operation) = if *status == DecoderStatus::NeedsOutput {
                (&[][..], DecodeOperation::Process)
            } else {
                let source = ready!(Pin::new(&mut *input).poll_fill_buf(cx))?;
                let operation = if source.is_empty() {
                    DecodeOperation::Finish
                } else {
                    DecodeOperation::Process
                };
                (source, operation)
            };
            let progress = session
                .process(source, output, operation)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            Pin::new(&mut *input).consume(progress.consumed);
            *status = progress.status;
            if progress.produced > 0 {
                return Poll::Ready(Ok(progress.produced));
            }
        }
    }
}
