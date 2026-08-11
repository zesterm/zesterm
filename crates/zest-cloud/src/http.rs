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

/// A configured `https://host[:port]/path`, in the three pieces [`post_json`]
/// takes.
///
/// # Why the splitting lives here rather than in the crate that calls it
///
/// The one caller is `zest_daemon::enroll`, which owns the control-plane URL
/// and could have split it itself — this crate cannot depend on `zest-daemon`,
/// so the `ControlPlane` impl is over there either way. What settled it is that
/// every rule below is a statement about *this transport* and none of them is
/// about enrolment: that the scheme must be `https` because [`post_json`]
/// speaks TLS and nothing else, and that an absent port means [`HTTPS_PORT`].
/// Splitting the URL in `zest-daemon` would put that default in a different
/// crate from the constant and the dialler that honour it, and a default port
/// that has quietly drifted from the client using it is wrong on exactly one
/// day: the first time anyone runs a control plane on 8443.
///
/// It is deliberately **not** a general URL type — no userinfo, no relative
/// resolution, no percent-decoding, no IPv6 literals. Each of those is refused
/// by name rather than half-implemented, on the same argument the module docs
/// make about chunked encoding: a URL parser that quietly mishandles a shape is
/// a request sent somewhere nobody chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// What is dialled, and the name the certificate is checked against.
    pub host: String,
    pub port: u16,
    /// The request target, query string included: everything from the first
    /// `/`, or `/` when the URL named only an origin.
    pub path: String,
}

impl Endpoint {
    /// Split a configured URL, or say what about it cannot be requested.
    pub fn parse(url: &str) -> io::Result<Self> {
        let Some(rest) = url.strip_prefix("https://") else {
            // `http://` is refused rather than dialled: a bearer token goes up
            // this request and another comes back down it, and a scheme that
            // silently downgraded them would be the one failure here nobody
            // sees, because it works.
            return Err(malformed(url, "it is not an https:// URL"));
        };
        if url.contains('#') {
            return Err(malformed(url, "a fragment is never sent to a server"));
        }

        let (authority, path) = match rest.find('/') {
            Some(cut) => (&rest[..cut], &rest[cut..]),
            None => (rest, "/"),
        };
        if authority.starts_with('[') {
            return Err(malformed(
                url,
                "a bracketed IPv6 literal is not supported; the control plane is reached by name",
            ));
        }
        if authority.contains('@') {
            return Err(malformed(url, "userinfo in a URL is not a credential this sends"));
        }
        // A query with no path before it is not a control-plane URL, and the
        // alternative to refusing it is a hostname with `?env=staging` inside,
        // which resolves nowhere and reports a DNS failure.
        if authority.contains('?') {
            return Err(malformed(url, "there is no path before the query"));
        }

        // `rsplit_once`, so the last colon separates the port -- safe only
        // because the bracketed form is refused above, which is the shape where
        // the last colon is inside the host.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .map_err(|_| malformed(url, &format!("{port:?} is not a port")))?,
            ),
            None => (authority, HTTPS_PORT),
        };
        if host.is_empty() {
            return Err(malformed(url, "there is no host in it"));
        }

        Ok(Self { host: host.to_string(), port, path: path.to_string() })
    }
}

/// Why a configured URL cannot be requested. `InvalidInput` rather than
/// `InvalidData`: this is something a person typed or a build set, not
/// something a peer sent.
fn malformed(url: &str, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{url:?} cannot be requested: {why}"))
}

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
            "the response gave no content-length, so where its body ends is not knowable. \
             (`transfer-encoding: identity` reaches here too: it is accepted, and it \
             supplies no length either.)",
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

    fn parsed(url: &str) -> Endpoint {
        Endpoint::parse(url).unwrap_or_else(|e| panic!("{url} should split: {e}"))
    }

    #[test]
    fn a_url_splits_into_the_three_things_a_request_needs() {
        assert_eq!(
            parsed("https://zesterm.sigx.workers.dev/api/enroll/claim"),
            Endpoint {
                host: "zesterm.sigx.workers.dev".into(),
                port: HTTPS_PORT,
                path: "/api/enroll/claim".into(),
            },
            "the deployed control plane's own URL is the one shape that must never be \
             mis-split, and an absent port is the scheme's"
        );
        assert_eq!(
            parsed("https://127.0.0.1:8787/api/enroll/claim"),
            Endpoint { host: "127.0.0.1".into(), port: 8787, path: "/api/enroll/claim".into() },
            "a port has to survive: `wrangler dev` serves on 8787, and losing it dials 443 \
             on the same box and reports the local Worker as unreachable"
        );
        assert_eq!(
            parsed("https://example.test").path,
            "/",
            "a URL naming only an origin still has to produce a request target"
        );
        assert_eq!(
            parsed("https://example.test/base/api/enroll/claim").path,
            "/base/api/enroll/claim",
            "a control plane behind a path prefix is routed on the whole path"
        );
        assert_eq!(
            parsed("https://example.test/claim?env=staging").path,
            "/claim?env=staging",
            "the query is part of the request target and is carried through verbatim"
        );
    }

    #[test]
    fn a_url_this_cannot_request_is_refused_rather_than_guessed_at() {
        // Every one of these has a plausible wrong answer -- downgrade to
        // plaintext, read the last colon out of an address, keep `user@` in the
        // hostname -- and each wrong answer addresses something nobody chose.
        // The first is the one that would work: a claim posted in the clear
        // succeeds, and the token it carries is on the wire for anyone.
        for (url, why) in [
            ("http://example.test/claim", "plaintext must not be silently dialled over TLS"),
            ("example.test/claim", "a URL with no scheme names no transport"),
            ("wss://example.test/claim", "this client speaks HTTP and not WebSocket"),
            ("https:///claim", "there is no host to dial or to check a certificate against"),
            ("https://example.test:https/claim", "a port that is not a number is not a port"),
            ("https://example.test:/claim", "an empty port is not the default"),
            // Both spellings, because only the second reaches the userinfo
            // check: the first is already refused for having `pw@example.test`
            // where its port should be, so testing it alone would leave that
            // check believed and unexercised.
            ("https://user:pw@example.test/claim", "userinfo is a credential this never sends"),
            ("https://user@example.test/claim", "`user@example.test` is not a host to dial"),
            ("https://[::1]:8787/claim", "the last colon in a bracketed address is not a port"),
            ("https://example.test/claim#frag", "a fragment never reaches a server"),
            ("https://example.test?env=staging", "a query where the path should start leaves \
              the query inside the hostname"),
        ] {
            let e = Endpoint::parse(url).err().unwrap_or_else(|| panic!("{url} was accepted: {why}"));
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "a configured URL is something a person typed, not something a peer sent: {e}"
            );
            assert!(
                e.to_string().contains(url),
                "the error has to quote the URL, or it reads as an outage rather than as a \
                 setting to fix: {e}"
            );
        }
    }
}
