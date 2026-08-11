//! One TLS connection, as two independently owned halves.
//!
//! [`serve`](../../zest_daemon/server/fn.serve.html) takes a reader and a
//! writer *separately* and drives them from two threads, because the reader
//! blocks in `read` for as long as the peer is quiet — which is most of the
//! time — while the writer pushes whatever a session produced. A
//! `rustls::StreamOwned` can be neither cloned nor split, so something has to
//! sit underneath and hand out the two halves. This is that something.
//!
//! # The invariant, and it is the whole design
//!
//! **The connection lock is never held across a syscall.** Ciphertext is
//! produced into a `Vec` under the `conn` lock and written to the socket after
//! the lock is released; plaintext arrives from the socket with **no lock held
//! at all** and is fed in afterwards. Lock order is `sock_w` → `conn`, never
//! the reverse.
//!
//! `sock_w` *is* held across both the encrypt and the send, and that is not an
//! oversight — see the second rejection below.
//!
//! ## Rejected: one owner thread handing out halves over channels
//!
//! Not implementable on blocking `std` I/O. The owner would have to wait on the
//! socket *and* on a channel at once and `std` has no `select`, so it is two
//! threads plus a connection mutex either way — at which point the channels
//! have added a queue depth nobody will tune, a memory bomb if unbounded, and a
//! second end-of-stream path that has to agree with the first.
//!
//! ## Rejected: two `Arc<Mutex<ClientConnection>>`, socket writes outside the lock
//!
//! Correct-looking, and silently corrupting. **TLS records carry an implicit
//! AEAD sequence number**, so two threads that each finish `write_tls` into
//! their own buffer and then race to `sock.write_all` put the records on the
//! wire out of order; the peer fails the MAC and kills the connection. The
//! symptom is a rare, unreproducible disconnect under concurrent
//! output-plus-keystroke load, which is the exact bug class the relay is being
//! built not to ship. Holding `sock_w` across `[encrypt, send]` is what makes
//! that pair atomic.
//!
//! # A fault latches *and* cuts
//!
//! Both halves, or neither. Latching alone is not enough: `serve` returns
//! `Ok(())` when a write fails and never joins its reader thread, so a write
//! fault that only set a flag would leave that thread parked in `sock.read`
//! for ever, holding a `Connection` and a `Registry` clone. That is a leak
//! that shows up after days and is diagnosed as a memory problem.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

/// How much ciphertext is taken off the socket in one `read`.
///
/// A TLS record is at most ~16 KiB, so this is "about one record" — big enough
/// that a full record rarely needs two syscalls, small enough to sit in every
/// reader without being noticed.
const CIPHERTEXT_CHUNK: usize = 16 * 1024;

/// Where the trust anchors come from.
///
/// Both are compiled in and the choice is made at runtime, because each one's
/// failure mode is the other's normal case. The platform verifier is the only
/// thing that accepts a corporate middlebox's re-signed certificate — on a
/// managed laptop, a bundled root list means the relay simply never connects.
/// Its own opposite is a minimal container image with no trust store at all,
/// where the bundled list is the only thing that works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Roots {
    /// The operating system's trust store. The default, and the right answer on
    /// a machine a person administers.
    #[default]
    Platform,
    /// Mozilla's root list, compiled into this binary.
    Bundled,
}

/// A connected, handshaken TLS connection, waiting to be split.
pub struct TlsDuplex {
    reader: TlsReader,
    writer: TlsWriter,
}

impl TlsDuplex {
    /// Dial `host:port`, verify it, and complete the handshake.
    ///
    /// Blocking and synchronous: it returns with the connection already usable
    /// or with the failure that stopped it, so a bad certificate is reported by
    /// the thing that dialled rather than by the first read a session makes.
    ///
    /// The TCP connect itself is bounded by the operating system and by nothing
    /// here — `connect_timeout` takes a resolved address, and resolving a name
    /// to try each of its addresses in turn is a policy the caller should own
    /// rather than one this quietly picks.
    pub fn connect(host: &str, port: u16, roots: Roots) -> io::Result<Self> {
        let sock = TcpStream::connect((host, port))?;
        // A terminal's traffic is keystroke-sized. Nagle would hold a keypress
        // back waiting for company that, on an interactive session, is the
        // next keypress.
        sock.set_nodelay(true)?;
        Self::over(sock, host, client_config(roots)?)
    }

    /// The same, over a socket somebody else connected.
    ///
    /// `server_name` is what the certificate is checked against, and is
    /// deliberately not derived from the socket's peer address: the name in the
    /// URL is the one being trusted.
    pub fn over(sock: TcpStream, server_name: &str, config: Arc<ClientConfig>) -> io::Result<Self> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{server_name}: {e}")))?;
        let mut conn = ClientConnection::new(config, name).map_err(io::Error::other)?;

        // The handshake runs before anything is shared, so it can use rustls'
        // own blocking helper and no lock at all.
        let mut handshake_sock = sock.try_clone()?;
        conn.complete_io(&mut handshake_sock).inspect_err(|_| {
            // A certificate failure arrives as an `InvalidData` io::Error whose
            // message is the alert; keeping the socket open to send the alert
            // is pointless once we are giving up on the connection.
            let _ = sock.shutdown(Shutdown::Both);
        })?;

        let sock_r = sock.try_clone()?;
        // The read timeout is armed *before* the reader can ever block, and
        // that ordering is the whole point — see `READ_POLL`.
        sock_r.set_read_timeout(Some(READ_POLL))?;
        let cut = sock.try_clone()?;
        let shared = Arc::new(Shared {
            conn: Mutex::new(conn),
            sock_w: Mutex::new(sock),
            cut,
            fault: Mutex::new(None),
            severed: AtomicBool::new(false),
        });
        Ok(Self {
            reader: TlsReader {
                shared: Arc::clone(&shared),
                sock: sock_r,
                buf: vec![0; CIPHERTEXT_CHUNK].into_boxed_slice(),
                filled: 0,
                taken: 0,
            },
            writer: TlsWriter { shared, scratch: Vec::new() },
        })
    }

    /// A handle that can sever this connection from anywhere.
    pub fn handle(&self) -> TlsHandle {
        TlsHandle { shared: Arc::clone(&self.writer.shared) }
    }

    /// Give up on a peer that stops answering mid-response.
    ///
    /// For a request/response exchange only. A read timeout is exactly the
    /// wrong thing on a *session* — `zest_daemon::lan`'s watchdog says why: a
    /// quiet connection is the healthy case and a timeout would kill it — which
    /// is why this is opt-in and why [`TlsHandle::cut`] exists beside it.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        // The option is on the socket, not on the handle, so setting it through
        // any clone reaches the reader's.
        self.writer.shared.sock_w.lock().expect("socket lock").set_read_timeout(timeout)
    }

    /// The two halves, to be owned by two threads.
    pub fn split(self) -> (TlsReader, TlsWriter) {
        (self.reader, self.writer)
    }
}

/// What both halves share: the TLS state, the write end of the socket, and the
/// one error that ends the connection for both of them.
/// How often a parked reader surfaces to ask whether the connection still
/// exists — and the only reason a cut works on Windows at all.
///
/// **Two rounds of Windows CI paid for this constant, so it is worth the
/// paragraph.** A cut has to unpark a reader sitting in `read`. On POSIX
/// `shutdown` does that. On Winsock it does not, and neither does setting
/// `SO_RCVTIMEO` at cut time: a socket timeout applies to calls made *after*
/// it is set, never to one already in flight. So a reader that parked before
/// the cut stayed parked, and `TlsHandle::cut` — which exists for the
/// handshake watchdog — did nothing on this project's primary platform, with
/// every call involved reporting success.
///
/// Arming the timeout up front is what makes it portable: the reader is never
/// blocked for longer than this, so it always comes back to check `severed`.
/// It is a poll, and that is a real cost stated rather than hidden — one
/// syscall per second per connection, against a watchdog that measures in tens
/// of seconds. The alternative on Windows is overlapped I/O, which is what
/// `zest-daemon`'s named pipes had to do for the same class of reason
/// (AGENTS.md, "Windows serializes I/O per handle"), and it is a great deal
/// more machinery than a second of latency is worth here.
const READ_POLL: Duration = Duration::from_secs(1);

struct Shared {
    conn: Mutex<ClientConnection>,
    sock_w: Mutex<TcpStream>,
    /// A third handle on the same socket, for `shutdown` alone.
    ///
    /// Third because the two that faulting has to interrupt are both busy at
    /// exactly the moment it runs: the reader is parked in `read` on its own
    /// handle, and the writer is holding `sock_w` when its `write_all` fails.
    cut: TcpStream,
    /// The first failure either half saw. `io::Error` is not `Clone`, so what
    /// is kept is enough to rebuild an equal one.
    fault: Mutex<Option<(io::ErrorKind, String)>>,
    /// Whether this connection has been ended, by a fault or by a cut.
    ///
    /// Read by the reader to tell a severing timeout from an ordinary one, so
    /// it exists for the same reason [`Shared::sever`] sets that timeout at
    /// all — see there.
    severed: AtomicBool,
}

impl Shared {
    /// End the connection for both halves, and return the error to report.
    ///
    /// The first fault wins: a write failure that then unparks the reader would
    /// otherwise be overwritten by the reader's "connection reset", which
    /// describes the cut rather than the cause.
    fn fault(&self, e: &io::Error) -> io::Error {
        let mut slot = self.fault.lock().expect("fault lock");
        let (kind, message) = slot.get_or_insert_with(|| (e.kind(), e.to_string())).clone();
        // Not conditional on being first: whoever faults, the socket must go
        // down, or a parked reader waits for a peer that will never speak.
        self.sever();
        io::Error::new(kind, message)
    }

    /// End the connection so that a reader parked in `read` comes back.
    ///
    /// **A `shutdown` is not enough, and assuming it was cost two Windows CI
    /// failures on this file's own tests.** POSIX wakes a blocked `recv` and
    /// returns 0 from one issued afterwards; Winsock does neither reliably, so
    /// the reader stayed parked and the watchdog this exists to serve would
    /// have done nothing on the project's primary platform.
    ///
    /// The `shutdown` still earns its place: on POSIX it makes the cut
    /// immediate rather than costing up to a [`READ_POLL`]. What it cannot be
    /// is the only mechanism.
    fn sever(&self) {
        self.severed.store(true, Ordering::Release);
        let _ = self.cut.shutdown(Shutdown::Both);
    }

    fn latched(&self) -> Option<io::Error> {
        self.fault
            .lock()
            .expect("fault lock")
            .as_ref()
            .map(|(kind, message)| io::Error::new(*kind, message.clone()))
    }

    /// Decrypted bytes, if rustls already has some. `None` means "ask the
    /// socket"; `Some(0)` means the peer said goodbye.
    fn drain(&self, out: &mut [u8]) -> io::Result<Option<usize>> {
        match self.conn.lock().expect("connection lock").reader().read(out) {
            Ok(n) => Ok(Some(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Hand rustls one bite of ciphertext, advancing `cipher` past what it
    /// took. `true` means the connection now owes the peer something — a
    /// key update, or an alert.
    ///
    /// One bite rather than a loop: `read_tls` fails outright once rustls'
    /// decrypted-plaintext buffer is full, and a loop that fed until the
    /// ciphertext ran out is precisely how it fills. Feeding is therefore only
    /// ever done immediately after a drain came up empty.
    fn feed(&self, cipher: &mut &[u8]) -> io::Result<bool> {
        let mut conn = self.conn.lock().expect("connection lock");
        if conn.read_tls(cipher)? == 0 {
            // `read_tls` returns the byte count its *reader* gave it, so zero
            // means end of input — for a slice, an empty slice. The caller
            // only reaches here with a non-empty one, so the remaining way to
            // see zero is rustls having stopped wanting ciphertext, which
            // after a `close_notify` it has. Whatever is behind that is not
            // ours to interpret; the caller learns the connection ended from
            // `drain` returning `Some(0)`.
            *cipher = &[];
            return Ok(false);
        }
        conn.process_new_packets().map_err(io::Error::other)?;
        Ok(conn.wants_write())
    }

    /// Encrypt whatever is queued and put it on the wire.
    ///
    /// `sock_w` is taken *first* and held across both, which is what keeps
    /// record order — see this module's second rejection.
    fn push(&self, scratch: &mut Vec<u8>, plaintext: Option<&[u8]>) -> io::Result<()> {
        let mut sock = self.sock_w.lock().expect("socket lock");
        scratch.clear();
        {
            let mut conn = self.conn.lock().expect("connection lock");
            if let Some(buf) = plaintext {
                conn.writer().write_all(buf)?;
            }
            while conn.wants_write() {
                conn.write_tls(scratch)?;
            }
        }
        if let Err(e) = sock.write_all(scratch) {
            return Err(self.fault(&e));
        }
        Ok(())
    }
}

/// The read half. Reads the socket holding no lock at all.
pub struct TlsReader {
    shared: Arc<Shared>,
    sock: TcpStream,
    buf: Box<[u8]>,
    /// How much of `buf` the last socket read filled, and how much of that
    /// rustls has taken. A record can straddle two socket reads and `read_tls`
    /// is free to take less than it is offered, so the remainder has to survive
    /// between calls.
    filled: usize,
    taken: usize,
}

impl Read for TlsReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if let Some(e) = self.shared.latched() {
            return Err(e);
        }
        loop {
            if let Some(n) = self.shared.drain(out).map_err(|e| self.shared.fault(&e))? {
                return Ok(n);
            }
            if self.taken < self.filled {
                let mut cipher = &self.buf[self.taken..self.filled];
                let owed = match self.shared.feed(&mut cipher) {
                    Ok(owed) => owed,
                    Err(e) => return Err(self.shared.fault(&e)),
                };
                self.taken = self.filled - cipher.len();
                if owed {
                    // A key update or an alert. It goes out through the same
                    // door as session output, so the writer's record ordering
                    // covers it too.
                    self.shared.push(&mut Vec::new(), None)?;
                }
                continue;
            }
            self.filled = match self.sock.read(&mut self.buf) {
                Ok(n) => n,
                // A signal, not a failure. `write_all` retries this for the
                // writer; nothing retries it here.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A `READ_POLL` elapsing. If the connection was severed while
                // this was parked, that is the end of the stream (or whatever
                // failed first); otherwise it is simply nothing to read yet,
                // and the loop asks again.
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    if !self.shared.severed.load(Ordering::Acquire) {
                        continue;
                    }
                    return match self.shared.latched() {
                        Some(e) => Err(e),
                        None => Ok(0),
                    };
                }
                Err(e) => return Err(self.shared.fault(&e)),
            };
            self.taken = 0;
            if self.filled == 0 {
                // The socket ended without a close_notify. That is `Ok(0)` and
                // not an error on purpose: `ws.rs`'s frame reader has its own
                // diagnostic for a peer that vanished mid-frame, and wrapping
                // this in an `UnexpectedEof` here would bury it under a second
                // one that names TLS instead of the frame.
                //
                // Unless something already failed — then this read is the cut
                // that failure performed, and the failure is the answer.
                return match self.shared.latched() {
                    Some(e) => Err(e),
                    None => Ok(0),
                };
            }
        }
    }
}

/// The write half. Owns nothing; everything it needs is behind `sock_w`.
pub struct TlsWriter {
    shared: Arc<Shared>,
    /// Ciphertext staging, reused. A terminal writes on every keystroke and on
    /// every frame, and an allocation per write is a cost with no upside.
    scratch: Vec<u8>,
}

impl Write for TlsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(e) = self.shared.latched() {
            return Err(e);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let Self { shared, scratch } = self;
        shared.push(scratch, Some(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(e) = self.shared.latched() {
            return Err(e);
        }
        let Self { shared, scratch } = self;
        shared.push(scratch, None)?;
        shared.sock_w.lock().expect("socket lock").flush()
    }
}

impl Drop for TlsWriter {
    /// Say goodbye, then close **the write direction only**.
    ///
    /// The half close is required rather than tidy: `ws.rs`'s `WsWriter::drop`
    /// writes a WebSocket close frame through this writer and then expects to
    /// read the peer's echo. `Shutdown::Both` here would make that read fail
    /// every time, and the clean close both ends are trying to perform would be
    /// reported as an abnormal one.
    fn drop(&mut self) {
        let _ = self.flush();
        {
            let mut conn = self.shared.conn.lock().expect("connection lock");
            conn.send_close_notify();
        }
        let Self { shared, scratch } = self;
        let _ = shared.push(scratch, None);
        let mut sock = shared.sock_w.lock().expect("socket lock");
        let _ = sock.flush();
        let _ = sock.shutdown(Shutdown::Write);
    }
}

/// Severs a connection from outside it, without owning either half.
///
/// This is what `zest_daemon::lan::Watchdog` needs and what a `TcpStream`
/// cannot give it here — a TLS stream is not a socket and cannot be
/// `try_clone`d into one, so the ability to cut has to be handed out
/// separately. Implementing that trait over this is one line, in the crate that
/// owns the trait.
///
/// It does **not** latch a fault: a cut is "this connection is over", and
/// `serve` unwinds through the same path it uses for a client that went away.
/// A failure that needs explaining sets the fault instead.
#[derive(Clone)]
pub struct TlsHandle {
    shared: Arc<Shared>,
}

impl TlsHandle {
    pub fn cut(&self) {
        self.shared.sever();
    }
}

/// The verifier and the root list, built once per [`Roots`].
///
/// Cached because building it is not free — on Linux the platform verifier
/// reads and parses the system trust store — and under ADR-009 a dial happens
/// per pipe, which is per attach rather than per process.
fn client_config(roots: Roots) -> io::Result<Arc<ClientConfig>> {
    static PLATFORM: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    static BUNDLED: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let cell = match roots {
        Roots::Platform => &PLATFORM,
        Roots::Bundled => &BUNDLED,
    };
    cell.get_or_init(|| build_config(roots).map_err(|e| e.to_string()))
        .as_ref()
        .map(Arc::clone)
        .map_err(|e| io::Error::other(e.clone()))
}

fn build_config(roots: Roots) -> Result<Arc<ClientConfig>, rustls::Error> {
    let builder = ClientConfig::builder();
    let mut config = match roots {
        Roots::Platform => {
            use rustls_platform_verifier::BuilderVerifierExt;
            builder.with_platform_verifier()?.with_no_client_auth()
        }
        Roots::Bundled => {
            let mut store = RootCertStore::empty();
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(store).with_no_client_auth()
        }
    };
    config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
    Ok(Arc::new(config))
}

/// Offered on every connection, and there is exactly one thing to offer.
///
/// Cloudflare's edge offers h2, and h2 has no `Upgrade:` handshake — a
/// WebSocket client that lets it be negotiated gets a response that names
/// nothing recognisable and reads as a broken relay. Saying `http/1.1` and
/// nothing else costs one line and removes the whole failure.
const ALPN_HTTP_1_1: &[u8] = b"http/1.1";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{dial, Peer, Say};
    use std::sync::mpsc;
    use std::thread;

    /// Long enough that a deadlock is not mistaken for a slow machine, short
    /// enough that a hung test is reported rather than waited out.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// How long a thread is given to reach its blocking `read` before the test
    /// starts relying on it being parked there.
    ///
    /// The race is one-sided: too short and a genuinely broken implementation
    /// might pass, never the reverse. There is nothing to observe instead —
    /// "parked in a syscall" is not a state a thread can publish.
    const SETTLE: Duration = Duration::from_millis(150);

    /// A reader on its own thread, reporting one `read` back over a channel.
    fn park(mut reader: TlsReader) -> mpsc::Receiver<io::Result<Vec<u8>>> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 256];
            let got = reader.read(&mut buf).map(|n| buf[..n].to_vec());
            let _ = tx.send(got);
        });
        thread::sleep(SETTLE);
        rx
    }

    #[test]
    fn plaintext_crosses_in_both_directions() {
        let peer = Peer::spawn();
        let (mut reader, mut writer) = dial(&peer).split();

        peer.say(Say::Expect(5));
        writer.write_all(b"hello").expect("the write half works at all");
        writer.flush().expect("flushing works at all");
        assert_eq!(peer.heard(), b"hello", "what the peer decrypted is what was written");

        peer.say(Say::Send(b"world".to_vec()));
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).expect("the read half works at all");
        assert_eq!(&buf, b"world", "what was decrypted is what the peer sent");
    }

    #[test]
    fn a_write_does_not_wait_for_the_parked_reader() {
        // The reason the halves are split at all. A single connection behind
        // one mutex passes every sequential test and deadlocks here, because
        // the reader holds the lock across a blocking `read` -- which is what a
        // reader does for as long as the peer is quiet, i.e. almost always.
        let peer = Peer::spawn();
        let (reader, mut writer) = dial(&peer).split();
        let parked = park(reader);

        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let got = writer.write_all(b"hello").and_then(|()| writer.flush());
            let _ = done.send(got);
            // Held, not dropped: dropping the writer would close the connection
            // and unpark the reader for a reason this test is not about.
            thread::sleep(PATIENCE);
        });
        peer.say(Say::Expect(5));
        finished
            .recv_timeout(PATIENCE)
            .expect("a write blocked behind a reader that was parked in a syscall")
            .expect("the write itself failed");
        assert_eq!(peer.heard(), b"hello", "the write went out while the reader was parked");

        peer.say(Say::Send(b"x".to_vec()));
        parked.recv_timeout(PATIENCE).expect("the parked read never woke").expect("read failed");
    }

    #[test]
    fn a_read_fault_surfaces_on_the_writer() {
        let peer = Peer::spawn();
        let (mut reader, mut writer) = dial(&peer).split();

        peer.say(Say::Corrupt);
        let mut buf = [0u8; 64];
        let read_err = reader.read(&mut buf).expect_err("a record that cannot authenticate is fatal");

        let write_err = writer.write_all(b"hello").expect_err(
            "the write half kept accepting bytes for a connection the read half had already lost",
        );
        assert_eq!(
            write_err.to_string(),
            read_err.to_string(),
            "the writer reported the cut rather than the decryption failure that caused it, \
             which is the difference between a diagnosable log line and 'connection reset'"
        );
    }

    #[test]
    fn a_write_fault_unparks_the_reader() {
        // The leak this prevents is silent and slow: `serve` returns `Ok(())`
        // on a write failure and never joins its reader thread, so a fault that
        // only latched would leave that thread in `sock.read` for ever holding
        // a `Connection` and a `Registry` clone.
        let peer = Peer::spawn();
        let duplex = dial(&peer);
        let tap = peer.client_socket();
        let (reader, mut writer) = duplex.split();
        let parked = park(reader);

        // Shutting down our own write direction is what makes the next `write`
        // fail on all three platforms without a timing guess. The peer is
        // parked on its command channel and neither reads the resulting EOF nor
        // closes, so the read direction stays exactly as quiet as it was.
        tap.shutdown(Shutdown::Write).expect("the test's own socket handle");
        let write_err = writer.write_all(b"hello").expect_err("the socket's write half is closed");

        let woke = parked
            .recv_timeout(PATIENCE)
            .expect("the reader was left parked in `read` by a fault on the other half");
        let read_err = woke.expect_err("the reader reported a clean end of stream for a failure");
        assert_eq!(
            read_err.to_string(),
            write_err.to_string(),
            "the reader woke with something other than the fault that woke it"
        );
    }

    #[test]
    fn dropping_the_writer_leaves_a_pending_read_alive() {
        // `WsWriter::drop` writes a close frame through this writer and then
        // expects to read the peer's echo, so the goodbye has to be a *half*
        // close. `Shutdown::Both` here would pass every test that checks the
        // bytes went out and break every clean close.
        let peer = Peer::spawn();
        let (reader, writer) = dial(&peer).split();
        let parked = park(reader);

        drop(writer);
        assert!(
            parked.recv_timeout(SETTLE).is_err(),
            "closing the write half took the read half with it, so a peer's reply to our \
             goodbye can never be read"
        );

        peer.say(Say::Bye);
        let woke = parked.recv_timeout(PATIENCE).expect("the peer closed and the read never woke");
        assert_eq!(
            woke.expect("a close_notify is not a failure").len(),
            0,
            "a clean close must read as end of stream"
        );
    }

    #[test]
    fn a_socket_that_ends_without_close_notify_reads_as_end_of_stream() {
        let peer = Peer::spawn();
        let (mut reader, _writer) = dial(&peer).split();

        peer.say(Say::Vanish);
        let mut buf = [0u8; 64];
        assert_eq!(
            reader.read(&mut buf).expect(
                "a truncated stream was reported as a TLS error, which buries the frame \
                 reader's own diagnostic under one that names the wrong layer"
            ),
            0,
            "end of stream is zero bytes"
        );
    }

    #[test]
    fn a_cut_that_lands_before_the_read_still_ends_it() {
        // The ordering the sibling test cannot control, made deterministic.
        //
        // `cut` and the reader race by construction: a watchdog fires on its own
        // thread and has no way to know whether the connection's reader has
        // reached `read` yet. On POSIX either order works — a `read` issued
        // after a `shutdown` returns 0 immediately. On Winsock it does not, and
        // this exact ordering left the reader parked until the test timed out,
        // twice, on CI. A watchdog that only cuts when it wins a race is not a
        // watchdog, and Windows is this project's primary platform.
        let peer = Peer::spawn();
        let duplex = dial(&peer);
        let handle = duplex.handle();
        let (mut reader, _writer) = duplex.split();

        handle.cut();

        let mut buf = [0u8; 64];
        assert_eq!(
            reader.read(&mut buf).expect("a cut is not a fault"),
            0,
            "a read issued after the cut hung instead of reporting the end of the stream"
        );
    }

    #[test]
    fn cutting_a_connection_unparks_its_reader_without_inventing_a_failure() {
        let peer = Peer::spawn();
        let duplex = dial(&peer);
        let handle = duplex.handle();
        let (reader, _writer) = duplex.split();
        let parked = park(reader);

        handle.cut();
        let woke = parked
            .recv_timeout(PATIENCE)
            .expect("a cut connection left its reader parked, which is the watchdog doing nothing");
        assert_eq!(
            woke.expect("a cut is not a fault: `serve` unwinds through 'the client went away'").len(),
            0,
            "a cut reads as end of stream"
        );
    }

    #[test]
    fn only_http_1_1_is_ever_offered() {
        // Cloudflare's edge offers h2, and an h2 connection has no `Upgrade:`
        // handshake at all -- the relay dial would fail with a response naming
        // nothing. This is one line to lose and expensive to diagnose.
        let config = build_config(Roots::Bundled).expect("the bundled roots always build");
        assert_eq!(
            config.alpn_protocols,
            vec![b"http/1.1".to_vec()],
            "a WebSocket client that lets h2 be negotiated cannot upgrade"
        );
    }
}
