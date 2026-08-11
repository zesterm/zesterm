//! One HTTPS request, and deliberately no more HTTP than that.
//!
//! The whole of the daemon's HTTP is a single POST of a small JSON body to a
//! Cloudflare Worker, whose answer is a status and a small JSON body
//! (`zest_daemon::enroll`). That is a request line, three headers and a
//! `content-length` read — about fifty lines — against an HTTP crate that
//! brings a connection pool, a redirect policy, a cookie jar and, in the async
//! ones, a runtime. `check-deps` names `ureq`, `reqwest` and `hyper` for the
//! same reason it names rustls: the cost of a dependency here is not its
//! download, it is that it becomes the thing every later caller reaches for.
//!
//! # What it does not do, and says so
//!
//! No redirects, no keep-alive (every exchange sends `connection: close` and
//! then drops the socket), no compression, no chunked transfer-encoding. The
//! last one is the only one a server can force on us, so it is *detected* and
//! reported by name rather than half-supported: a decoder that mistook the
//! chunk-size line for the body would hand `zest_daemon::enroll` a token that
//! is a hex length, and that failure is one nobody would think to look for.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

use crate::tls::{Roots, TlsDuplex};

/// The scheme's port, and the only one this ever infers.
pub const HTTPS_PORT: u16 = 443;

/// How long the whole exchange may take before it is given up on.
///
/// Safe here where it would be wrong on a session: a request/response exchange
/// that has gone quiet is stuck by definition, which is the distinction
/// [`TlsDuplex::set_read_timeout`] exists to draw.
const DEADLINE: Duration = Duration::from_secs(30);

/// Refuse a response body larger than this.
///
/// The control plane answers with an object holding an account name and a
/// token. Anything at this scale is a mistake or a hostile peer, and neither is
/// worth an allocation the size of it.
const MAX_BODY: usize = 1 << 20;

/// And a head larger than this. The two are enforced as one ceiling on
/// everything read from the socket, which is also what stops a peer that never
/// sends a newline from growing a `read_line` without bound.
const MAX_HEAD: usize = 64 * 1024;

/// What came back.
///
/// The status is carried rather than folded into an error, because "was this
/// refused" is a question about enrolment and not about HTTP — a 409 on a spent
/// code and a 403 on a bad signature are two different things to tell a person.
/// Only a request that never completed is an `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// POST `body` to `https://host[:port]path` as JSON.
pub fn post_json(host: &str, port: u16, path: &str, body: &str, roots: Roots) -> io::Result<Response> {
    let duplex = TlsDuplex::connect(host, port, roots)?;
    duplex.set_read_timeout(Some(DEADLINE))?;
    let (reader, writer) = duplex.split();
    exchange(reader, writer, &authority(host, port), path, body)
}

/// The same exchange over anything readable and writable.
///
/// Split out from [`post_json`] because everything that is worth testing here
/// is the bytes, and none of it is TLS: the tests drive this over a `Vec` and a
/// canned response, so a wrong header or a body read one byte short fails in
/// milliseconds with no socket, no certificate and no network.
pub fn exchange(
    reader: impl Read,
    mut writer: impl Write,
    authority: &str,
    path: &str,
    body: &str,
) -> io::Result<Response> {
    // One `write_all`, not five: each one is a TLS record with its own AEAD tag
    // and its own syscall, and a request split across records is also a request
    // whose shape is visible to anyone counting packets.
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         host: {authority}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n{body}",
        body.len(),
    );
    writer.write_all(request.as_bytes())?;
    writer.flush()?;
    read_response(reader)
}

fn read_response(reader: impl Read) -> io::Result<Response> {
    let mut reader = BufReader::new(reader.take(MAX_HEAD as u64 + MAX_BODY as u64));

    let status = status_line(&line(&mut reader)?)?;

    let mut length: Option<usize> = None;
    loop {
        let header = line(&mut reader)?;
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a response header with no colon in it: {header:?}"),
            ));
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match name.as_str() {
            "content-length" => {
                length = Some(value.parse().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("a content-length that is not a number: {value:?}"),
                    )
                })?);
            }
            // Named, not guessed at. See the module docs: a body read as if it
            // were not chunked starts with a hex length and looks like a
            // corrupt token rather than like an unimplemented encoding.
            "transfer-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "the control plane answered with transfer-encoding: {value}, which this \
                         client does not implement — it reads a body by content-length only"
                    ),
                ));
            }
            _ => {}
        }
    }

    // Absent is an error rather than an empty body: every answer this makes a
    // decision from has one, and treating "no length" as "no body" turns a
    // response nobody read into a successful enrolment with no token in it.
    let Some(length) = length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the response had neither a content-length nor a transfer-encoding, so where it \
             ends is not knowable",
        ));
    };
    if length > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {length}-byte response body, which is not an answer to this request"),
        ));
    }

    let mut body = vec![0u8; length];
    // `read_exact`, so a connection that dies mid-body is a failure and not a
    // short JSON document that fails to parse three layers away.
    reader.read_exact(&mut body).map_err(|e| {
        io::Error::new(e.kind(), format!("the response body ended after less than {length} bytes"))
    })?;
    let body = String::from_utf8(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("a non-UTF-8 body: {e}")))?;
    Ok(Response { status, body })
}

/// `HTTP/1.1 200 OK` → `200`. The reason phrase is not read: it is advisory,
/// may be empty, and nothing downstream can act on it.
fn status_line(line: &str) -> io::Result<u16> {
    let mut parts = line.trim_end_matches(['\r', '\n']).splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not an HTTP/1.x response: {line:?}"),
        ));
    }
    parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("no status code in {line:?}")))
}

/// One CRLF-terminated line, or the end of the stream as a failure.
///
/// A truncated head is never `Ok`: the caller is looking for a status and a
/// length, and a stream that stopped before either has not answered.
fn line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the response head ended without a blank line: the connection closed part-way \
             through it, or it ran past the ceiling this reads",
        ));
    }
    Ok(line)
}

/// What goes in `host:`. The port is omitted when it is the scheme's, because
/// some origins route on the header verbatim and `:443` is a different string.
fn authority(host: &str, port: u16) -> String {
    if port == HTTPS_PORT {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response with a head, so a test only has to say the interesting part.
    fn answered(head: &str, body: &str) -> Vec<u8> {
        format!("{head}\r\n{body}").into_bytes()
    }

    fn ok_response() -> Vec<u8> {
        answered("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 9\r\n", "{\"ok\":1}\n")
    }

    #[test]
    fn the_request_is_the_one_a_worker_will_route() {
        let mut sent = Vec::new();
        exchange(&ok_response()[..], &mut sent, "zesterm.example", "/api/enroll/claim", "{\"code\":\"C\"}")
            .expect("a well-formed response");
        assert_eq!(
            String::from_utf8(sent).expect("the request is ASCII"),
            "POST /api/enroll/claim HTTP/1.1\r\n\
             host: zesterm.example\r\n\
             content-type: application/json\r\n\
             content-length: 12\r\n\
             connection: close\r\n\
             \r\n{\"code\":\"C\"}",
            "a missing host routes to whatever the edge serves by default and a wrong \
             content-length hangs the origin waiting for a body that already arrived"
        );
    }

    #[test]
    fn a_refusal_comes_back_as_a_status_and_not_as_an_error() {
        // `ControlPlane::post_json`'s contract: a completed request that said
        // no is `Ok`, because a 409 on a spent code and a 403 on a bad
        // signature are two different things to tell a person, and a transport
        // that collapsed them into a transport error would make the difference
        // unrecoverable.
        let canned = answered("HTTP/1.1 409 Conflict\r\ncontent-length: 17\r\n", "{\"error\":\"spent\"}");
        let got = exchange(&canned[..], &mut Vec::new(), "h", "/p", "{}").expect("a refusal completed");
        assert_eq!(
            got,
            Response { status: 409, body: "{\"error\":\"spent\"}".into() },
            "the status and the reason both have to survive: one of them is what a person is told"
        );
    }

    #[test]
    fn a_chunked_response_is_refused_by_name() {
        let canned = answered("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n", "8\r\n{\"ok\":1}\r\n0\r\n\r\n");
        let e = exchange(&canned[..], &mut Vec::new(), "h", "/p", "{}")
            .expect_err("a body this client cannot decode was decoded anyway");
        assert!(
            e.to_string().contains("chunked"),
            "the one encoding a server can impose has to name itself in the error, or the \
             symptom is a token that is really a hex chunk length: {e}"
        );
    }

    #[test]
    fn a_body_that_stops_early_is_a_failure_and_not_a_short_answer() {
        let canned = answered("HTTP/1.1 200 OK\r\ncontent-length: 64\r\n", "{\"token\":\"abc");
        let e = exchange(&canned[..], &mut Vec::new(), "h", "/p", "{}")
            .expect_err("a truncated body was returned as if it were the whole one");
        assert_eq!(
            e.kind(),
            io::ErrorKind::UnexpectedEof,
            "a connection that died mid-body must not surface as JSON that fails to parse \
             three layers away: {e}"
        );
    }

    #[test]
    fn a_response_with_no_length_at_all_is_refused() {
        let canned = answered("HTTP/1.1 200 OK\r\n", "{\"token\":\"abc\"}");
        let e = exchange(&canned[..], &mut Vec::new(), "h", "/p", "{}")
            .expect_err("a body of unknowable length was read as if it had one");
        assert!(
            e.to_string().contains("content-length"),
            "reading no body at all would report a successful enrolment holding no token: {e}"
        );
    }

    #[test]
    fn the_default_port_is_left_out_of_the_host_header() {
        // Some origins route on the header verbatim, and `zesterm.example:443`
        // is a different string from `zesterm.example`.
        assert_eq!(authority("zesterm.example", HTTPS_PORT), "zesterm.example");
        assert_eq!(authority("zesterm.example", 8443), "zesterm.example:8443");
    }

    #[test]
    fn a_reply_that_is_not_http_is_refused_rather_than_guessed_at() {
        // Three space-separated words are not an HTTP response, and the second
        // one being a number does not make them one. Something else answering
        // on the port has to be a failure, because the alternative is a status
        // this client made up and a caller branching on it.
        let canned = answered("ICY 200 OK\r\ncontent-length: 0\r\n", "");
        let e = exchange(&canned[..], &mut Vec::new(), "h", "/p", "{}")
            .expect_err("something that is not a response was parsed as one");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }
}
