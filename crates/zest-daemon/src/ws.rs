//! Serving browsers: the same protocol, carried over WebSocket.
//!
//! A browser cannot open a unix socket or dial raw TCP, and those are the only
//! transports the daemon speaks. This module adds the third: an RFC 6455
//! WebSocket listener that carries the **identical length-prefixed MessagePack
//! stream** the other two carry. `frame.rs` deliberately keeps the u32 prefix
//! on every transport — including this one, where WebSocket's own framing makes
//! it redundant — precisely so this module can be a byte pipe and
//! [`serve`](crate::server::serve) can stay byte-for-byte untouched.
//!
//! # Why the server half is hand-rolled
//!
//! `serve` requires independently owned read and write halves — its doc comment
//! calls the split load-bearing, and it is: the reader blocks in `read` while
//! the writer pushes session output. Sync `tungstenite` cannot be split that
//! way. One `WebSocket` behind a mutex is exactly the deadlock `serve`
//! describes; two instances over `try_clone`d streams share no write lock, so
//! the reading side's auto-queued pong can interleave mid-frame with a
//! keyframe from the writing side and corrupt the stream. The *server* half of
//! RFC 6455 is meanwhile the easy half: no masking on send, no fragmentation
//! of outgoing messages, no extensions (permessage-deflate is never
//! negotiated), no subprotocols. What is genuinely fiddly — masked payloads,
//! continuation frames, control frames arriving mid-fragmentation, close
//! semantics — is a few hundred lines, each testable against RFC vectors, and
//! hand-rolling buys the one thing tungstenite could not provide: a
//! frame-granular shared write lock, so a pong reply and a data frame can
//! never interleave. `tungstenite` still earns its keep as a *dev-dependency*:
//! the end-to-end tests drive this server with an independent implementation
//! rather than a mirror of itself.
//!
//! # Why there is no Origin check
//!
//! Authorization here is the Ed25519 challenge–response, not ambient
//! authority. There are no cookies and no credentialed state, so the
//! cross-site attack an Origin allowlist defends against has nothing to steal:
//! a hostile page that connects gets what a LAN port-scanner gets — a
//! challenge it cannot sign, bounded by the same mid-handshake cap and
//! per-address cooldowns as everyone else. An allowlist would meanwhile have
//! to enumerate every origin the web client will ever be served from, and a
//! wrong list is an outage where a right one adds nothing a signature does
//! not. The header is logged at debug for diagnostics.
//!
//! # Why there is no TLS
//!
//! Deferred, deliberately. `http://localhost` is a secure context and may open
//! `ws://`, which covers the local sidecar; the internet path is a Cloudflare
//! Tunnel (roadmap M4) where the browser speaks `wss://` to the edge and the
//! tunnel speaks plaintext to this origin, so origin TLS never needs to exist
//! on that path. LAN `ws://` is unencrypted — which is parity with the raw-TCP
//! LAN transport today, not a regression. Transport encryption is a known open
//! item on the roadmap, not something to half-do here.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha1::{Digest, Sha1};

use crate::auth::{Auth, Authenticator};
use crate::lan::{accept_hardened, LanListener};
use crate::server::{serve_lan, Registry};
use crate::{DaemonConfig, DaemonError};

/// The port browsers dial when nothing else is asked for.
///
/// One above [`lan::DEFAULT_PORT`](crate::lan::DEFAULT_PORT), so one firewall
/// conversation covers both, and unused by anything else in the tree.
pub const DEFAULT_PORT: u16 = 7718;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;
const FIN: u8 = 0x80;
const RSV: u8 = 0x70;
const MASKED: u8 = 0x80;

/// Control frames may not exceed this (RFC 6455 §5.5), and a peer that sends
/// one bigger has lost the plot badly enough to hang up on.
const MAX_CONTROL_PAYLOAD: u64 = 125;

/// The HTTP upgrade request may not exceed this. The watchdog bounds how long
/// a peer may take to send it; this bounds how much memory it costs while it
/// does.
const MAX_UPGRADE_HEADER: usize = 16 * 1024;

/// Which end of the connection this codec is.
///
/// The asymmetry is masking: a client masks every frame it sends and a server
/// sends everything unmasked, and each side must *fail the connection* when
/// the other gets it wrong (RFC 6455 §5.1) — so the same codec serves both
/// roles by knowing which one it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Client,
}

impl Role {
    /// The mask for a frame this side sends, freshly random per frame as the
    /// RFC requires of clients. `None` for a server, which must not mask.
    fn send_mask(self) -> io::Result<Option<[u8; 4]>> {
        match self {
            Self::Server => Ok(None),
            Self::Client => {
                let nonce = zest_mesh::identity::Nonce::random()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let b = nonce.as_bytes();
                Ok(Some([b[0], b[1], b[2], b[3]]))
            }
        }
    }
}

fn protocol_error(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("websocket: {what}"))
}

/// Write one whole frame. Always called with the shared write lock held, which
/// is the invariant that keeps a pong reply from landing inside a keyframe.
fn write_frame(
    w: &mut impl Write,
    opcode: u8,
    payload: &[u8],
    mask: Option<[u8; 4]>,
) -> io::Result<()> {
    let mut header = [0u8; 14];
    header[0] = FIN | opcode;
    let mask_bit = if mask.is_some() { MASKED } else { 0 };
    let len = payload.len();
    let mut n = if len < 126 {
        header[1] = mask_bit | len as u8;
        2
    } else if let Ok(short) = u16::try_from(len) {
        header[1] = mask_bit | 126;
        header[2..4].copy_from_slice(&short.to_be_bytes());
        4
    } else {
        header[1] = mask_bit | 127;
        header[2..10].copy_from_slice(&(len as u64).to_be_bytes());
        10
    };
    if let Some(key) = mask {
        header[n..n + 4].copy_from_slice(&key);
        n += 4;
        w.write_all(&header[..n])?;
        let mut masked = payload.to_vec();
        for (i, b) in masked.iter_mut().enumerate() {
            *b ^= key[i & 3];
        }
        w.write_all(&masked)?;
    } else {
        w.write_all(&header[..n])?;
        w.write_all(payload)?;
    }
    w.flush()
}

/// The read half: WebSocket frames in, a plain byte stream out.
///
/// Data-frame payloads stream through in arrival order — no per-message
/// reassembly, because the zest-proto layer's own length prefix means
/// [`FrameReader`](zest_proto::frame::FrameReader) does the assembly and
/// enforces `MAX_FRAME`. That is also why a hostile 2⁶³ declared length
/// allocates nothing here: payload is consumed incrementally in bounded reads.
///
/// Control frames are handled inline: a ping is answered under the shared
/// write lock, a close is echoed once and reported as EOF, and anything
/// malformed — a text frame on this binary protocol, an unmasked client
/// frame, reserved bits with no extension negotiated — returns `Err`, which
/// `serve`'s reader treats as "framing lost; closing": exactly the RFC's
/// fail-the-connection.
struct WsReader<R: Read, W: Write> {
    stream: R,
    /// For pong and close replies only; data frames go out through
    /// [`WsWriter`] on the same lock.
    replies: Arc<Mutex<W>>,
    role: Role,
    /// Raw socket bytes, seeded with whatever arrived behind the HTTP
    /// upgrade. Losing those is the classic first-frame bug.
    buf: Vec<u8>,
    pos: usize,
    /// Payload bytes still owed by the current data frame.
    remaining: u64,
    mask: [u8; 4],
    /// Offset into the mask, mod 4, carried across `read` calls because one
    /// frame's payload rarely fits one caller buffer.
    mask_at: usize,
    /// A data frame with FIN clear was seen and its continuation has not
    /// finished. Only consulted to validate opcodes: payload bytes stream out
    /// the same either way.
    in_message: bool,
    closed: bool,
}

impl<R: Read, W: Write> WsReader<R, W> {
    fn refill(&mut self) -> io::Result<()> {
        // Cheap and bounded: at a refill the unconsumed tail is at most one
        // frame header or one control payload, so this drain is a memmove of
        // a few dozen bytes, and it is what keeps `buf` from growing without
        // bound over a long session.
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut chunk = [0u8; 16 * 1024];
        let n = self.stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "websocket: the peer went away mid-frame",
            ));
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn available(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn need(&mut self, n: usize) -> io::Result<()> {
        while self.available() < n {
            self.refill()?;
        }
        Ok(())
    }

    /// Parse frames until a data frame's payload begins or the connection
    /// closes. Control frames never reach the caller.
    fn next_data_frame(&mut self) -> io::Result<()> {
        loop {
            self.need(2)?;
            let b0 = self.buf[self.pos];
            let b1 = self.buf[self.pos + 1];
            let len7 = b1 & 0x7f;
            let masked = b1 & MASKED != 0;
            let mut header = 2 + match len7 {
                126 => 2,
                127 => 8,
                _ => 0,
            };
            if masked {
                header += 4;
            }
            self.need(header)?;

            if b0 & RSV != 0 {
                return Err(protocol_error("reserved bits set, but no extension was negotiated"));
            }
            match (self.role, masked) {
                // §5.1: each side must fail the connection, not quietly cope.
                (Role::Server, false) => {
                    return Err(protocol_error("a client frame arrived unmasked"))
                }
                (Role::Client, true) => return Err(protocol_error("the server masked a frame")),
                _ => {}
            }

            let at = self.pos + 2;
            let len: u64 = match len7 {
                126 => u64::from(u16::from_be_bytes([self.buf[at], self.buf[at + 1]])),
                127 => u64::from_be_bytes([
                    self.buf[at],
                    self.buf[at + 1],
                    self.buf[at + 2],
                    self.buf[at + 3],
                    self.buf[at + 4],
                    self.buf[at + 5],
                    self.buf[at + 6],
                    self.buf[at + 7],
                ]),
                n => u64::from(n),
            };
            let key = if masked {
                // Only meaningful — and only computable — when the mask bit is
                // set: an unmasked header can be 2 bytes, and `pos + 2 - 4`
                // underflows. Found by the first real client-role run, not by
                // the server-role tests, whose peers always mask.
                let key_at = self.pos + header - 4;
                [
                    self.buf[key_at],
                    self.buf[key_at + 1],
                    self.buf[key_at + 2],
                    self.buf[key_at + 3],
                ]
            } else {
                // XOR with zero is the identity, so unmasking can be
                // unconditional.
                [0; 4]
            };
            self.pos += header;

            let fin = b0 & FIN != 0;
            match b0 & 0x0f {
                OP_BINARY => {
                    if self.in_message {
                        return Err(protocol_error("a new message began inside a fragmented one"));
                    }
                    self.in_message = !fin;
                    self.remaining = len;
                    self.mask = key;
                    self.mask_at = 0;
                    return Ok(());
                }
                OP_CONTINUATION => {
                    if !self.in_message {
                        return Err(protocol_error("a continuation frame with nothing to continue"));
                    }
                    if fin {
                        self.in_message = false;
                    }
                    self.remaining = len;
                    self.mask = key;
                    self.mask_at = 0;
                    return Ok(());
                }
                OP_TEXT => {
                    // The protocol is MessagePack; a text frame is a client
                    // that dialled the wrong thing, and pretending to cope
                    // would corrupt the byte stream.
                    return Err(protocol_error("a text frame on a binary protocol"));
                }
                op @ (OP_CLOSE | OP_PING | OP_PONG) => {
                    if !fin {
                        return Err(protocol_error("a fragmented control frame"));
                    }
                    if len > MAX_CONTROL_PAYLOAD {
                        return Err(protocol_error("an oversized control frame"));
                    }
                    let len = len as usize;
                    self.need(len)?;
                    let mut payload = self.buf[self.pos..self.pos + len].to_vec();
                    self.pos += len;
                    for (i, b) in payload.iter_mut().enumerate() {
                        *b ^= key[i & 3];
                    }
                    match op {
                        OP_PING => {
                            let mask = self.role.send_mask()?;
                            let mut w = self.replies.lock().expect("ws write lock");
                            write_frame(&mut *w, OP_PONG, &payload, mask)?;
                        }
                        OP_PONG => {}
                        _ => {
                            // Echo the close (status code and all) so the
                            // peer's close completes cleanly, then report EOF:
                            // `serve` unwinds through its ordinary "the client
                            // went away" path.
                            let mask = self.role.send_mask()?;
                            {
                                let mut w = self.replies.lock().expect("ws write lock");
                                let _ = write_frame(&mut *w, OP_CLOSE, &payload, mask);
                            }
                            self.closed = true;
                            return Ok(());
                        }
                    }
                }
                op => return Err(protocol_error(&format!("unknown opcode {op:#x}"))),
            }
        }
    }
}

impl<R: Read, W: Write> Read for WsReader<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.closed {
                return Ok(0);
            }
            if self.remaining == 0 {
                self.next_data_frame()?;
                continue;
            }
            if self.available() == 0 {
                self.refill()?;
            }
            let have = self.available() as u64;
            let n = self.remaining.min(have).min(out.len() as u64) as usize;
            for (i, slot) in out[..n].iter_mut().enumerate() {
                *slot = self.buf[self.pos + i] ^ self.mask[(self.mask_at + i) & 3];
            }
            self.pos += n;
            self.mask_at = (self.mask_at + n) & 3;
            self.remaining -= n as u64;
            return Ok(n);
        }
    }
}

/// The write half: a plain byte stream in, WebSocket frames out.
///
/// `write` only buffers; `flush` emits **one binary frame holding everything
/// buffered**. That mapping is correct because of how `serve` writes: one
/// `write_all` per encoded zest-proto message, one `flush` per wake batch — so
/// the buffer at flush time always holds a whole number of zest-proto frames,
/// a frame never splits across WebSocket messages, and one message may carry a
/// batch. A comment beside `serve`'s flush points back here; if that write
/// pattern ever changes, this coupling is the thing to re-examine, and the
/// `one_ws_message_per_flush` test is the thing that goes red.
struct WsWriter<W: Write> {
    shared: Arc<Mutex<W>>,
    buf: Vec<u8>,
    role: Role,
}

impl<W: Write> Write for WsWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mask = self.role.send_mask()?;
        let mut w = self.shared.lock().expect("ws write lock");
        write_frame(&mut *w, OP_BINARY, &self.buf, mask)?;
        self.buf.clear();
        Ok(())
    }
}

impl<W: Write> Drop for WsWriter<W> {
    fn drop(&mut self) {
        // Best-effort 1000 "normal closure": a browser told goodbye reports a
        // clean close instead of an abnormal one. Nothing to do if the peer is
        // already gone.
        if let (Ok(mask), Ok(mut w)) = (self.role.send_mask(), self.shared.lock()) {
            let _ = write_frame(&mut *w, OP_CLOSE, &1000u16.to_be_bytes(), mask);
        }
    }
}

/// Standard base64, encode only.
///
/// Hand-rolled rather than a crate whose entire use here is the 28 characters
/// of `Sec-WebSocket-Accept`, and pinned by the RFC's own worked example in
/// the tests below.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// The `Sec-WebSocket-Accept` digest (RFC 6455 §1.3). A protocol checksum,
/// not a security boundary — which is why SHA-1 is fine here and nowhere else.
fn accept_key(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut hasher = Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(GUID.as_bytes());
    base64(&hasher.finalize())
}

/// Read from `stream` until the blank line that ends an HTTP header block.
///
/// Returns the header bytes and **whatever arrived after them** — a client
/// that pipelines its first frame behind the upgrade is entirely legal, and
/// dropping those bytes is the classic bug this signature exists to prevent.
fn read_header_block(stream: &mut impl Read) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut head: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // The whole buffer every pass, because the terminator can straddle a
        // read boundary and the block is capped at 16 KiB — rescanning is
        // cheaper than being clever about offsets was correct.
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            let leftover = head.split_off(i + 4);
            return Ok((head, leftover));
        }
        if head.len() > MAX_UPGRADE_HEADER {
            return Err(protocol_error("the HTTP header block never ended"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer went away before finishing its HTTP headers",
            ));
        }
        head.extend_from_slice(&chunk[..n]);
    }
}

/// Does a `Connection`/`Upgrade`-style header contain `token`? Both are
/// comma-separated token lists and case-insensitive — browsers really do
/// disagree about casing here.
fn has_token(value: &str, token: &str) -> bool {
    value.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// Perform the server side of the HTTP upgrade.
///
/// On success the `101` has been written and the connection speaks WebSocket;
/// the returned bytes are whatever the client sent behind its headers, which
/// must seed the frame parser. On failure a best-effort `400` is written and
/// the error says why — and the caller's watchdog accounting records it as a
/// failed handshake, which is what feeds the per-address cooldown.
fn accept_upgrade(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let (head, leftover) = read_header_block(stream)?;
    let text = std::str::from_utf8(&head)
        .map_err(|_| protocol_error("the HTTP request was not UTF-8"))?;

    let refuse = |stream: &mut TcpStream, why: &str| -> io::Error {
        let _ = stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
        protocol_error(why)
    };

    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or("");
    if !request.starts_with("GET ") {
        return Err(refuse(stream, "the upgrade must be a GET"));
    }

    let mut key = None;
    let mut version_ok = false;
    let mut upgrade_ok = false;
    let mut connection_ok = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        // Header names are case-insensitive and browsers exercise that.
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-websocket-key" => key = Some(value.to_string()),
            "sec-websocket-version" => version_ok = has_token(value, "13"),
            "upgrade" => upgrade_ok = has_token(value, "websocket"),
            "connection" => connection_ok = has_token(value, "upgrade"),
            // Logged, never enforced — the module docs say why.
            "origin" => tracing::debug!(origin = %value, "websocket upgrade"),
            _ => {}
        }
    }

    if !upgrade_ok || !connection_ok {
        return Err(refuse(stream, "not a websocket upgrade"));
    }
    if !version_ok {
        return Err(refuse(stream, "unsupported websocket version"));
    }
    let Some(key) = key else {
        return Err(refuse(stream, "no Sec-WebSocket-Key"));
    };

    // No `Sec-WebSocket-Protocol`, no `Sec-WebSocket-Extensions`: neither is
    // echoed, so neither is ever negotiated. In particular permessage-deflate
    // stays off — compressed frames would put a decompressor between a
    // hostile peer and the frame parser for bytes MessagePack barely
    // compresses anyway.
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );
    stream.write_all(response.as_bytes())?;
    Ok(leftover)
}

/// Split an upgraded stream into the halves `serve` wants.
fn split(
    stream: TcpStream,
    role: Role,
    leftover: Vec<u8>,
) -> io::Result<(WsReader<TcpStream, TcpStream>, WsWriter<TcpStream>)> {
    let write_half = stream.try_clone()?;
    let shared = Arc::new(Mutex::new(write_half));
    Ok((
        WsReader {
            stream,
            replies: Arc::clone(&shared),
            role,
            buf: leftover,
            pos: 0,
            remaining: 0,
            mask: [0; 4],
            mask_at: 0,
            in_message: false,
            closed: false,
        },
        WsWriter { shared, buf: Vec::new(), role },
    ))
}

/// A bound WebSocket port, not yet serving.
///
/// Wraps [`LanListener`] for `bind`'s port-fallback behaviour and reuses the
/// same hardened accept loop, so this transport cannot take a public port
/// without taking the posture that comes with one.
pub struct WsListener {
    inner: LanListener,
}

impl WsListener {
    /// Take the port. Falls back to an ephemeral one exactly as the LAN
    /// listener does; advertise [`Self::local_addr`], never the request.
    pub fn bind(bind_addr: &str, port: u16) -> Result<Self, DaemonError> {
        Ok(Self { inner: LanListener::bind(bind_addr, port)? })
    }

    /// Shorten the handshake deadline. For tests.
    #[must_use]
    pub fn with_handshake_timeout(self, timeout: Duration) -> Self {
        Self { inner: self.inner.with_handshake_timeout(timeout) }
    }

    /// What was actually bound.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    /// Accept until the process ends.
    ///
    /// The HTTP upgrade runs on the connection's own thread with the
    /// handshake watchdog already armed, so a peer that opens a socket and
    /// never sends its request — or sends half of one forever — is cut by the
    /// same deadline as a TCP client that never says `Hello`.
    pub fn serve_forever(
        self,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: Arc<Authenticator>,
    ) -> Result<(), DaemonError> {
        tracing::info!(addr = %self.local_addr(), "serving WebSocket clients");
        let (listener, handshake_timeout) = self.inner.into_parts();
        accept_hardened(listener, handshake_timeout, move |mut stream, peer, watchdog, slot| {
            let leftover = accept_upgrade(&mut stream)
                .map_err(|e| DaemonError::Transport(format!("upgrade failed: {e}")))?;
            let (reader, writer) = split(stream, Role::Server, leftover)
                .map_err(|e| DaemonError::Transport(e.to_string()))?;
            // `Auth::Proof`: same rule as the LAN — the trust store decides,
            // and this connection may not approve other devices.
            serve_lan(
                reader,
                writer,
                config.clone(),
                Arc::clone(&registry),
                Auth::Proof(Arc::clone(&auth)),
                peer,
                watchdog,
                slot,
            )
        })
    }
}

/// The client role, `pub` for diagnostics and native tools — the layer-testing
/// story (`examples/attach.rs --ws`) needs a client that is *not* a browser,
/// so a wrong renderer and a wrong transport stay distinguishable. Browsers
/// bring their own implementation; nothing here is for them.
pub mod client {
    use super::{
        accept_key, base64, protocol_error, read_header_block, split, Role, WsReader, WsWriter,
    };
    use std::io::{self, Read, Write};
    use std::net::TcpStream;

    /// Dial the daemon's WebSocket port: perform the HTTP upgrade, verify the
    /// accept digest, and hand back the two halves a protocol loop wants.
    pub fn connect(
        mut stream: TcpStream,
    ) -> io::Result<(impl Read + Send + 'static, impl Write + Send + 'static)> {
        let host = stream
            .peer_addr()
            .map_or_else(|_| "unknown".to_string(), |a| a.to_string());
        let nonce = zest_mesh::identity::Nonce::random()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let key = base64(&nonce.as_bytes()[..16]);
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;

        let (head, leftover) = read_header_block(&mut stream)?;
        let text = std::str::from_utf8(&head)
            .map_err(|_| protocol_error("the HTTP response was not UTF-8"))?;
        let mut lines = text.split("\r\n");
        let status = lines.next().unwrap_or("");
        if !status.starts_with("HTTP/1.1 101") {
            return Err(protocol_error(&format!("the upgrade was refused: {status}")));
        }
        let accept = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-accept"))
            .map(|(_, v)| v.trim().to_string())
            .ok_or_else(|| protocol_error("no Sec-WebSocket-Accept in the response"))?;
        // A wrong digest means something between us and the daemon answered
        // the HTTP request without speaking WebSocket — a proxy, usually.
        if accept != accept_key(&key) {
            return Err(protocol_error("the accept digest is wrong"));
        }

        let (reader, writer): (WsReader<TcpStream, TcpStream>, WsWriter<TcpStream>) =
            split(stream, Role::Client, leftover)?;
        Ok((reader, writer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    type TestReader = WsReader<Cursor<Vec<u8>>, Vec<u8>>;

    /// A reader over canned bytes, with replies captured in memory.
    fn reader_over(role: Role, bytes: Vec<u8>) -> (TestReader, Arc<Mutex<Vec<u8>>>) {
        let replies = Arc::new(Mutex::new(Vec::new()));
        (
            WsReader {
                stream: Cursor::new(bytes),
                replies: Arc::clone(&replies),
                role,
                buf: Vec::new(),
                pos: 0,
                remaining: 0,
                mask: [0; 4],
                mask_at: 0,
                in_message: false,
                closed: false,
            },
            replies,
        )
    }

    fn masked_frame(opcode: u8, fin: bool, key: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(if fin { FIN | opcode } else { opcode });
        assert!(payload.len() < 126, "test helper only builds short frames");
        out.push(MASKED | payload.len() as u8);
        out.extend_from_slice(&key);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
        out
    }

    #[test]
    fn the_rfc_worked_example_digest_comes_out_right() {
        // RFC 6455 §1.3 pins both the base64 helper and the SHA-1 wiring at
        // once; if either is wrong, no browser will ever finish an upgrade.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_handles_every_tail_length() {
        // 20 bytes (a SHA-1 digest) is the case that matters; the others make
        // sure the padding logic is not accidentally digest-shaped.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn a_masked_payload_is_unmasked_for_the_caller() {
        let frame = masked_frame(OP_BINARY, true, [0xa1, 0xb2, 0xc3, 0xd4], b"hello world");
        let (mut r, _) = reader_over(Role::Server, frame);
        let mut out = vec![0u8; 32];
        let n = r.read(&mut out).expect("read");
        assert_eq!(&out[..n], b"hello world", "the mask must come off exactly once");
    }

    #[test]
    fn a_payload_read_in_small_pieces_keeps_the_mask_aligned() {
        // One frame's payload rarely fits one caller buffer, so the mask
        // offset carries across `read` calls. Off by one and every fourth
        // byte of a keyframe is garbage.
        let frame = masked_frame(OP_BINARY, true, [0x11, 0x22, 0x33, 0x44], b"abcdefghij");
        let (mut r, _) = reader_over(Role::Server, frame);
        let mut out = Vec::new();
        let mut piece = [0u8; 3];
        loop {
            match r.read(&mut piece) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&piece[..n]),
            }
        }
        assert_eq!(out, b"abcdefghij");
    }

    #[test]
    fn an_unmasked_client_frame_fails_the_connection() {
        // §5.1. Coping silently would mean a proxy that strips masking goes
        // unnoticed until it corrupts something harder to see.
        let mut frame = vec![FIN | OP_BINARY, 3];
        frame.extend_from_slice(b"abc");
        let (mut r, _) = reader_over(Role::Server, frame);
        assert!(r.read(&mut [0u8; 8]).is_err());
    }

    #[test]
    fn a_text_frame_fails_the_connection() {
        let frame = masked_frame(OP_TEXT, true, [1, 2, 3, 4], b"hi");
        let (mut r, _) = reader_over(Role::Server, frame);
        assert!(r.read(&mut [0u8; 8]).is_err(), "the protocol is MessagePack, never text");
    }

    #[test]
    fn reserved_bits_fail_the_connection() {
        // No extension is ever negotiated, so RSV means a peer that thinks it
        // agreed permessage-deflate with someone.
        let mut frame = masked_frame(OP_BINARY, true, [1, 2, 3, 4], b"x");
        frame[0] |= 0x40;
        let (mut r, _) = reader_over(Role::Server, frame);
        assert!(r.read(&mut [0u8; 8]).is_err());
    }

    #[test]
    fn a_fragmented_message_streams_through_in_order() {
        // The byte-pipe property: fragmentation is a transport detail, and the
        // zest-proto FrameReader above never sees a seam.
        let mut bytes = masked_frame(OP_BINARY, false, [9, 8, 7, 6], b"first ");
        bytes.extend(masked_frame(OP_CONTINUATION, true, [1, 1, 1, 1], b"second"));
        let (mut r, _) = reader_over(Role::Server, bytes);
        let mut out = Vec::new();
        r.read_to_end(&mut out).ok();
        assert_eq!(out, b"first second");
    }

    #[test]
    fn a_ping_between_fragments_is_answered_without_disturbing_the_data() {
        // §5.5: control frames may arrive mid-message. The pong must go out
        // and the data stream must not notice.
        let mut bytes = masked_frame(OP_BINARY, false, [9, 8, 7, 6], b"first ");
        bytes.extend(masked_frame(OP_PING, true, [2, 2, 2, 2], b"marco"));
        bytes.extend(masked_frame(OP_CONTINUATION, true, [1, 1, 1, 1], b"second"));
        let (mut r, replies) = reader_over(Role::Server, bytes);
        let mut out = Vec::new();
        r.read_to_end(&mut out).ok();
        assert_eq!(out, b"first second", "the ping must not interrupt the payload");
        let sent = replies.lock().expect("lock").clone();
        assert_eq!(sent[0], FIN | OP_PONG);
        assert_eq!(&sent[2..], b"marco", "a pong echoes the ping's payload, unmasked");
    }

    #[test]
    fn a_close_is_echoed_and_reported_as_eof() {
        let bytes = masked_frame(OP_CLOSE, true, [5, 5, 5, 5], &1000u16.to_be_bytes());
        let (mut r, replies) = reader_over(Role::Server, bytes);
        let mut out = [0u8; 8];
        assert_eq!(r.read(&mut out).expect("read"), 0, "a clean close is EOF, not an error");
        let sent = replies.lock().expect("lock").clone();
        assert_eq!(sent[0], FIN | OP_CLOSE, "the close must be echoed so the peer's close completes");
    }

    #[test]
    fn an_absurd_declared_length_allocates_nothing() {
        // A hostile 2⁶³-byte header must not reserve memory: payload is
        // streamed, and the only allocation is the parser's small buffer.
        let mut frame = vec![FIN | OP_BINARY, MASKED | 127];
        frame.extend_from_slice(&(u64::MAX >> 1).to_be_bytes());
        frame.extend_from_slice(&[1, 2, 3, 4]); // mask key
        frame.extend_from_slice(&[0u8; 64]); // far fewer bytes than declared
        let (mut r, _) = reader_over(Role::Server, frame);
        let mut out = [0u8; 32];
        let n = r.read(&mut out).expect("the first bytes stream out normally");
        assert!(n > 0);
        // The source runs dry long before the declared length: an honest
        // UnexpectedEof, not an allocation or a hang.
        let mut rest = Vec::new();
        assert!(r.read_to_end(&mut rest).is_err());
    }

    #[test]
    fn a_client_role_reader_accepts_a_short_unmasked_frame() {
        // The exact shape that panicked on the first real run: a server frame
        // under 126 bytes has a two-byte header and no mask key, and computing
        // the key offset anyway underflowed. Server-role tests cannot see this
        // because their peers always mask.
        let mut frame = vec![FIN | OP_BINARY, 5];
        frame.extend_from_slice(b"short");
        let (mut r, _) = reader_over(Role::Client, frame);
        let mut out = [0u8; 8];
        let n = r.read(&mut out).expect("read");
        assert_eq!(&out[..n], b"short");
    }

    #[test]
    fn a_continuation_with_nothing_to_continue_fails() {
        let frame = masked_frame(OP_CONTINUATION, true, [1, 2, 3, 4], b"lost");
        let (mut r, _) = reader_over(Role::Server, frame);
        assert!(r.read(&mut [0u8; 8]).is_err());
    }

    #[test]
    fn one_ws_message_per_flush_holding_whole_frames() {
        // The contract with `serve`'s writer: write_all per message, flush per
        // batch — so everything between two flushes coalesces into exactly one
        // binary frame and a zest-proto frame never splits across messages.
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut w = WsWriter { shared: Arc::clone(&shared), buf: Vec::new(), role: Role::Server };
        w.write_all(b"one").expect("write");
        w.write_all(b"two").expect("write");
        w.flush().expect("flush");
        w.write_all(b"three").expect("write");
        w.flush().expect("flush");

        let sent = shared.lock().expect("lock").clone();
        assert_eq!(sent[0], FIN | OP_BINARY);
        assert_eq!(sent[1], 6, "server frames are unmasked and the batch is one payload");
        assert_eq!(&sent[2..8], b"onetwo");
        assert_eq!(sent[8], FIN | OP_BINARY);
        assert_eq!(sent[9], 5);
        assert_eq!(&sent[10..15], b"three");
    }

    #[test]
    fn an_empty_flush_sends_nothing() {
        // serve flushes once per wake even when a wake produced no output;
        // that must not become a stream of empty frames.
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut w = WsWriter { shared: Arc::clone(&shared), buf: Vec::new(), role: Role::Server };
        w.flush().expect("flush");
        assert!(shared.lock().expect("lock").is_empty());
    }

    #[test]
    fn leftover_bytes_from_the_upgrade_are_not_lost() {
        // A client may pipeline its first frame behind the HTTP request; those
        // bytes arrive in the header read and must seed the parser.
        let frame = masked_frame(OP_BINARY, true, [3, 1, 4, 1], b"pipelined");
        let (head, tail) = {
            let mut all = b"GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n".to_vec();
            all.extend(&frame);
            let mut cursor = Cursor::new(all);
            read_header_block(&mut cursor).expect("split")
        };
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(tail, frame, "bytes behind the header block belong to the frame parser");
    }

    #[test]
    fn the_header_terminator_is_found_across_a_read_boundary() {
        // \r\n\r\n split over two reads is exactly what a slow network does.
        struct TwoReads(Vec<Vec<u8>>);
        impl Read for TwoReads {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                out[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }
        let mut src = TwoReads(vec![b"GET / HTTP/1.1\r\n\r".to_vec(), b"\nrest".to_vec()]);
        let (head, tail) = read_header_block(&mut src).expect("split");
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(tail, b"rest");
    }

    #[test]
    fn header_matching_survives_odd_casing_and_token_lists() {
        // Browsers legitimately send `Connection: keep-alive, Upgrade` and
        // any casing they like.
        assert!(has_token("keep-alive, Upgrade", "upgrade"));
        assert!(has_token("WEBSOCKET", "websocket"));
        assert!(!has_token("keep-alive", "upgrade"));
    }
}
