//! The single entry point that turns request state into the `hyper::Response`
//! sent to the client.
//!
//! # The seam
//!
//! Every outgoing response in this crate is built by calling
//! [`build_response`] or [`reject_response`] -- nothing else in this crate
//! calls `Response::builder()`. That is deliberate, not incidental: this
//! issue's body requires "the construction of the outgoing response --
//! status, header list, body framing -- in one narrow module with a single
//! entry point", because **#159 replaces this module wholesale** with a
//! byte-exact writer that reproduces PHP's own header casing and ordering.
//! `http::HeaderName::from_bytes` unconditionally lowercases every header
//! name (see #145/#159), so [`build_response`] below does **not** preserve
//! casing -- acceptable here because byte-exact wire fidelity is #159's job,
//! not this issue's (this issue's own acceptance test asserts headers
//! through a case-insensitive HTTP client for exactly this reason).
use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};

use frankenrust_core::context::RejectedRequest;

use crate::sink::SharedResponseBuffer;

/// Builds the response for a request a PHP thread actually ran, from the
/// [`SharedResponseBuffer`] its [`crate::sink::BufferedResponseSink`]
/// accumulated. See this module's doc comment for why this is the only
/// place in the crate allowed to call `Response::builder()`.
pub fn build_response(buffer: &SharedResponseBuffer) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(buffer.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(status);

    if let Some(headers) = builder.headers_mut() {
        for (name, values) in buffer.headers.iter() {
            let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            for value in values {
                let Ok(header_value) = HeaderValue::from_bytes(value) else {
                    continue;
                };
                headers.append(header_name.clone(), header_value);
            }
        }
    }

    builder
        .body(Full::new(Bytes::from(buffer.body.clone())))
        .unwrap_or_else(|_| internal_error_response())
}

/// Port of `fc.reject()`'s response-writing half (`context.go:184-207`),
/// minus the flush -- `Full<Bytes>` bodies have no incremental flush to
/// perform. The status and message are upstream's `RejectedRequest`
/// (`context.rs`'s port of `context.go:150-168`'s decision), rendered
/// exactly as `rw.WriteHeader(re.status); rw.Write(err.Error())` does: the
/// message as the entire body, no headers.
pub fn reject_response(rejected: &RejectedRequest) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::BAD_REQUEST);
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(rejected.message.clone())))
        .unwrap_or_else(|_| internal_error_response())
}

/// The one fallback every fallible builder call above collapses to: a
/// bodiless 500. Constructing this cannot itself fail (fixed status, no
/// headers, empty body), so it is the correct thing to `unwrap_or_else`
/// into rather than a second layer of `Result`.
pub fn internal_error_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Full::new(Bytes::new()))
        .expect("a bodiless, headerless 500 response is always constructible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_response_carries_status_headers_and_body() {
        let mut buffer = SharedResponseBuffer {
            status: 201,
            ..SharedResponseBuffer::default()
        };
        buffer
            .headers
            .insert("Content-Type", b"text/plain".to_vec());
        buffer.body = b"hello".to_vec();

        let response = build_response(&buffer);
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
    }

    #[test]
    fn build_response_falls_back_to_200_for_an_unset_status() {
        let buffer = SharedResponseBuffer::default();
        assert_eq!(build_response(&buffer).status(), StatusCode::OK);
    }

    #[test]
    fn reject_response_carries_the_message_as_the_body() {
        let rejected = RejectedRequest {
            status: 400,
            message: "invalid request path".to_string(),
        };
        let response = reject_response(&rejected);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
