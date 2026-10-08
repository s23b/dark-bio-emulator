// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! COBS-framed HTTP messages over native IPC, leaving retries to each caller.

use std::fmt::Write as _;
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::time::Instant;

use darkbio_cobs as cobs;

use super::local::Stream;

/// Read one zero-delimited COBS message, bounding encoded and decoded sizes.
pub(super) fn read_frame(stream: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let encoded_limit = cobs::encode_buffer(limit);
    let mut encoded = Vec::new();
    BufReader::new(stream)
        .take(encoded_limit as u64 + 1)
        .read_until(0, &mut encoded)?;
    if encoded.last() != Some(&0) {
        return Err(io::Error::new(
            if encoded.len() > encoded_limit {
                io::ErrorKind::InvalidData
            } else {
                io::ErrorKind::UnexpectedEof
            },
            "unterminated local COBS message",
        ));
    }
    encoded.pop();
    let mut raw = vec![0; cobs::decode_buffer(encoded.len())];
    let length = cobs::decode(&encoded, &mut raw)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if length == 0 || length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local message size",
        ));
    }
    raw.truncate(length);
    Ok(raw)
}

/// Send one complete native message with a zero delimiter.
pub(super) fn write_frame(mut stream: impl Write, raw: &[u8]) -> io::Result<()> {
    let mut encoded = vec![0; cobs::encode_buffer(raw.len()) + 1];
    let length = cobs::encode(raw, &mut encoded)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    encoded[length] = 0;
    stream.write_all(&encoded[..=length])
}

/// A complete response whose status is interpreted by the calling operation.
#[derive(Debug)]
pub(super) struct Response {
    /// HTTP status returned by the peer.
    pub(super) status: u16,
    /// Complete, unencoded response body.
    pub(super) body: Vec<u8>,
}

/// Exchange one request within a shared deadline and total response size limit.
pub(super) fn exchange(
    mut stream: Stream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    deadline: Instant,
    max_response: u64,
) -> io::Result<Response> {
    // Request components come from fixed routes and validated caller metadata
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").unwrap();
    }
    head.push_str("\r\n");

    // Partial writes and reads consume one budget, including connection setup
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream.set_write_timeout(remaining);
    stream.set_read_timeout(remaining);
    let mut raw = head.into_bytes();
    if let Some(body) = body {
        raw.extend_from_slice(body);
    }
    write_frame(&mut stream, &raw)?;
    let raw = read_frame(stream, max_response as usize)?;
    parse_response(&raw)
}

/// Parse a complete framed response without a second body framing scheme.
pub(super) fn parse_response(raw: &[u8]) -> io::Result<Response> {
    let split = match raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
        Some(split) => split + 4,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local HTTP response headers",
            ));
        }
    };
    // Parse the complete header block before interpreting its body framing
    let mut fields = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut fields);
    if !matches!(
        response.parse(&raw[..split]),
        Ok(httparse::Status::Complete(_))
    ) || response.version != Some(0)
        || !response.code.is_some_and(|code| (100..600).contains(&code))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local HTTP response headers",
        ));
    }

    // The COBS delimiter defines the body boundary
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("Transfer-Encoding")
            || header.name.eq_ignore_ascii_case("Content-Length")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported local HTTP framing header",
            ));
        }
    }
    Ok(Response {
        status: response.code.unwrap(),
        body: raw[split..].to_vec(),
    })
}

/// Response framing and exchanges over native connections.
#[cfg(test)]
mod tests {
    use super::super::local::{self, Server};
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Native readers share COBS vectors, including zeros and malformed blocks.
    #[test]
    fn test_frame_vectors() {
        for (wire, raw) in [
            (
                b"\x0e{\"version\":1}\0".as_slice(),
                b"{\"version\":1}".as_slice(),
            ),
            (b"\x05\"\xc3\xa9\"\0", b"\"\xc3\xa9\""),
            (b"\x03ab\x03cd\0", b"ab\0cd"),
        ] {
            assert_eq!(read_frame(wire, 64).unwrap(), raw);
            let mut encoded = Vec::new();
            write_frame(&mut encoded, raw).unwrap();
            assert_eq!(encoded, wire);
        }
        for wire in [b"\0".as_slice(), b"\x01\0", b"\x03A\0"] {
            assert_eq!(
                read_frame(wire, 64).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for wire in [b"".as_slice(), b"\x0e{\"version\":1}", b"\x03A"] {
            assert_eq!(
                read_frame(wire, 64).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    /// Both decoded overflow and an unbounded delimiter-free stream are refused.
    #[test]
    fn test_frame_limits() {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &[b'x'; 1024]).unwrap();
        assert_eq!(read_frame(encoded.as_slice(), 1024).unwrap(), [b'x'; 1024]);
        for wire in [encoded, [vec![1; 1026], vec![0]].concat(), vec![1; 1030]] {
            assert_eq!(
                read_frame(wire.as_slice(), 1023).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    /// A complete frame is readable before its peer closes the connection.
    #[test]
    fn test_frame_completion_does_not_wait_for_eof() {
        let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
        let mut stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
        write_frame(&mut stream, b"GET /test HTTP/1.0\r\n\r\n").unwrap();
        let mut peer = server.recv().unwrap().into_writer();
        write_frame(&mut peer, b"HTTP/1.0 200 OK\r\n\r\n{}\0").unwrap();
        assert_eq!(
            read_frame(stream, 8192).unwrap(),
            b"HTTP/1.0 200 OK\r\n\r\n{}\0"
        );
        drop(peer);
    }

    /// Complete replies preserve their status and body for the calling operation.
    #[test]
    fn test_complete_responses() {
        for (raw, status, body) in [
            ("HTTP/1.0 204 No Content\r\n\r\n", 204, ""),
            ("HTTP/1.0 200 OK\r\n\r\n{\"a\":1}", 200, "{\"a\":1}"),
            (
                "HTTP/1.0 400 Bad Request\r\n\r\ninvalid input",
                400,
                "invalid input",
            ),
        ] {
            let response = parse_response(raw.as_bytes()).unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.body, body.as_bytes());
        }
    }

    /// Connection loss is recognizable at every boundary of the headers and body.
    #[test]
    fn test_truncated_responses() {
        let mut reply = Vec::new();
        write_frame(&mut reply, b"HTTP/1.0 200 OK\r\n\r\n{\"a\":1}").unwrap();
        for end in 0..reply.len() {
            let err = read_frame(&reply[..end], 8192).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{end}");
        }
    }

    /// Malformed or ambiguous framing cannot be mistaken for connection loss.
    #[test]
    fn test_invalid_responses() {
        for reply in [
            "garbage",
            "HTTP?",
            "HTTP/1.0 200 OK\r\n",
            "HTTP/1.0 invalid\r\n\r\n",
            "HTTP/1.0 999 Unknown\r\n\r\n",
            "HTTP/1.1 200 OK\r\n\r\n{}",
            "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: invalid\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\nextra",
            "HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            let err = parse_response(reply.as_bytes()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{reply}");
        }
    }

    /// Native exchanges preserve request metadata and enforce the total reply limit.
    #[test]
    fn test_exchange_framing_and_response_limit() {
        let response = b"HTTP/1.0 200 OK\r\n\r\n{}";
        let body = "{\"disk\":\"démo.ark\"}".as_bytes();
        for limit in [response.len() as u64, response.len() as u64 - 1] {
            // Inspect bytes received by the peer before returning a fixed response
            let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
            let stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
            let worker = thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(request.url(), "/v1/instances");
                assert_eq!(request.body(), body);
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Content-Type")
                            && header.value.as_str() == "application/json")
                );
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("X-Ark-Generation")
                            && header.value.as_str() == "42")
                );
                assert!(
                    request
                        .headers()
                        .iter()
                        .all(|header| !header.field.equiv("Content-Length"))
                );
                write_frame(request.into_writer(), response).unwrap();
            });
            // Accept a reply exactly at the limit and reject one byte beyond it
            let reply = exchange(
                stream,
                "POST",
                "/v1/instances",
                &[("X-Ark-Generation", "42")],
                Some(body),
                Instant::now() + Duration::from_secs(2),
                limit,
            );
            worker.join().unwrap();
            if limit == response.len() as u64 {
                assert_eq!(reply.unwrap().body, b"{}");
            } else {
                assert_eq!(reply.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
        }
    }
}
