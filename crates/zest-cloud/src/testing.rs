//! A scripted TLS peer at the other end of a loopback socket.
//!
//! [`TlsDuplex`] is a client, so exercising it needs a server — and a server
//! that does only what it is told, when it is told. Every test in `tls` turns
//! on what the far end does at a precise moment: stay silent while a write goes
//! out, send a record that cannot authenticate, close cleanly, vanish without a
//! goodbye.
//!
//! So the peer reads nothing, writes nothing and closes nothing until a [`Say`]
//! arrives on its channel. That is load-bearing rather than tidy: several tests
//! assert something about a connection whose far end is **quiet**, and the
//! quiet is what a real server would helpfully break — draining its socket in a
//! background loop makes "the reader is parked because the peer has nothing to
//! say" untestable, because the peer would always have said something.
//!
//! It talks to [`TlsDuplex::over`] rather than [`TlsDuplex::connect`], which is
//! the whole reason `over` takes a config: a self-signed certificate reaches
//! the client as an injected root store, and no test needs the machine it runs
//! on to trust anything.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig, ServerConnection};

use crate::tls::TlsDuplex;

/// The name on the peer's certificate, and the name the client is asked to
/// check it against.
///
/// A DNS name rather than `127.0.0.1`, because an address literal in a SAN is a
/// different path through webpki than the one a relay hostname takes, and the
/// harness should not be the only place that path is exercised.
const PEER_NAME: &str = "localhost";

/// How long [`Peer::heard`] waits for the peer to report.
///
/// Generous: this bounds a hang so it is reported as a failure rather than
/// waited out, and it is never reached by a working implementation.
const PATIENCE: Duration = Duration::from_secs(5);

/// One instruction for the peer. It does nothing at all until it gets one.
pub enum Say {
    /// Read exactly this many bytes of plaintext and hand them to
    /// [`Peer::heard`].
    Expect(usize),
    /// Encrypt these bytes and put them on the wire.
    Send(Vec<u8>),
    /// Send a record that cannot possibly authenticate.
    Corrupt,
    /// Send `close_notify` and stay where it is — the clean close, with the
    /// socket still open so a reply could still be read.
    Bye,
    /// Close the socket with no `close_notify`: a peer that lost power.
    Vanish,
}

/// The far end of one TLS connection, and the controls for it.
pub struct Peer {
    addr: SocketAddr,
    /// The peer's own self-signed certificate, which [`dial`] hands the client
    /// as its only trust anchor.
    root: CertificateDer<'static>,
    orders: Sender<Say>,
    /// Behind a `Mutex` only because [`Peer::heard`] reads it through `&self`;
    /// nothing here is used from two threads at once.
    reports: Mutex<Receiver<Vec<u8>>>,
    /// The *client's* socket, kept so a test can interfere with the connection
    /// from underneath — see [`Peer::client_socket`].
    client: Mutex<Option<TcpStream>>,
}

impl Peer {
    /// Bind a loopback listener, mint a certificate for it, and wait for one
    /// connection.
    pub fn spawn() -> Self {
        let issued = rcgen::generate_simple_self_signed([PEER_NAME.to_string()])
            .expect("a self-signed certificate for a fixed name");
        let root = issued.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![root.clone()], key)
            .expect("a freshly minted certificate and the key that signed it");
        // The client offers exactly one protocol and nothing else; a server
        // that offered none would leave the negotiation untested here, which is
        // the half of `only_http_1_1_is_ever_offered` a unit assertion cannot
        // reach.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
        let addr = listener.local_addr().expect("the port that was just bound");
        let (orders, taking) = mpsc::channel();
        let (reporting, reports) = mpsc::channel();
        thread::spawn(move || run(&listener, Arc::new(config), &taking, &reporting));

        Self { addr, root, orders, reports: Mutex::new(reports), client: Mutex::new(None) }
    }

    /// Tell the peer to do one thing. Returns as soon as it is queued.
    pub fn say(&self, say: Say) {
        self.orders.send(say).expect("the peer stopped taking orders");
    }

    /// What the last [`Say::Expect`] read. Blocks until it has read it all.
    pub fn heard(&self) -> Vec<u8> {
        self.reports
            .lock()
            .expect("report channel")
            .recv_timeout(PATIENCE)
            .expect("the peer never reported the plaintext it was told to expect")
    }

    /// Another handle on the socket the *client* is using.
    ///
    /// The only way to fault a connection from outside it without guessing at
    /// timing: shutting down the client's own write direction makes its next
    /// `write` fail on all three platforms, immediately, while leaving the read
    /// direction exactly as quiet as it was.
    pub fn client_socket(&self) -> TcpStream {
        self.client
            .lock()
            .expect("client socket")
            .as_ref()
            .expect("nothing has dialled this peer yet")
            .try_clone()
            .expect("a second handle on a connected socket")
    }
}

/// Connect to `peer` and complete a handshake, trusting only its certificate.
pub fn dial(peer: &Peer) -> TlsDuplex {
    let sock = TcpStream::connect(peer.addr).expect("the peer's listener");
    sock.set_nodelay(true).expect("nodelay on a loopback socket");
    *peer.client.lock().expect("client socket") =
        Some(sock.try_clone().expect("a second handle on a connected socket"));

    let mut roots = RootCertStore::empty();
    roots.add(peer.root.clone()).expect("the peer's certificate is its own trust anchor");
    let mut config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    TlsDuplex::over(sock, PEER_NAME, Arc::new(config)).expect("the handshake with the test peer")
}

/// Accept one connection, handshake, then do as told until the orders run out.
fn run(
    listener: &TcpListener,
    config: Arc<ServerConfig>,
    orders: &Receiver<Say>,
    reporting: &Sender<Vec<u8>>,
) {
    let (mut sock, _) = listener.accept().expect("a client to dial");
    let mut conn = ServerConnection::new(config).expect("a server config that already built");
    conn.complete_io(&mut sock).expect("the handshake with the client under test");

    for order in orders {
        let outcome = match order {
            Say::Expect(n) => recv_exact(&mut conn, &mut sock, n).map(|got| {
                let _ = reporting.send(got);
            }),
            Say::Send(bytes) => {
                conn.writer().write_all(&bytes).expect("rustls' own plaintext buffer");
                flush(&mut conn, &mut sock)
            }
            Say::Corrupt => corrupt(&mut conn, &mut sock),
            Say::Bye => {
                conn.send_close_notify();
                flush(&mut conn, &mut sock)
            }
            Say::Vanish => {
                // No `close_notify`, and the write direction goes down with the
                // read one: the client must see a bare FIN, which is what a
                // machine that lost power leaves behind.
                let _ = sock.shutdown(Shutdown::Both);
                return;
            }
        };
        if let Err(e) = outcome {
            // Nothing else can be done on this connection, and a panic here
            // would be reported on the wrong thread. The test blocks on
            // `heard()` or on its own reader and fails with its own message.
            eprintln!("test peer gave up: {e}");
            return;
        }
    }
}

/// Read exactly `n` bytes of plaintext, taking ciphertext from the socket only
/// when rustls has none left to decrypt.
fn recv_exact(conn: &mut ServerConnection, sock: &mut TcpStream, n: usize) -> io::Result<Vec<u8>> {
    let mut got = Vec::with_capacity(n);
    while got.len() < n {
        let mut chunk = [0u8; 1024];
        let want = (n - got.len()).min(chunk.len());
        match conn.reader().read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(k) => got.extend_from_slice(&chunk[..k]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if conn.read_tls(sock)? == 0 {
                    break;
                }
                conn.process_new_packets().map_err(io::Error::other)?;
                // A key update the client initiated is answered here or not at
                // all; this peer has no other moment at which it writes.
                flush(conn, sock)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Put everything rustls has queued on the wire.
fn flush(conn: &mut ServerConnection, sock: &mut TcpStream) -> io::Result<()> {
    while conn.wants_write() {
        conn.write_tls(sock)?;
    }
    sock.flush()
}

/// Send one record with a bit flipped in its ciphertext.
///
/// A hand-written record would be rejected on its framing, which is a different
/// failure with a different error and would let a client that never decrypts
/// anything pass. This one is real — right content type, right length, right
/// sequence number — and the only thing wrong with it is that it cannot
/// authenticate.
fn corrupt(conn: &mut ServerConnection, sock: &mut TcpStream) -> io::Result<()> {
    conn.writer().write_all(b"tampered").expect("rustls' own plaintext buffer");
    let mut record = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut record)?;
    }
    *record.last_mut().expect("rustls produced a record for buffered plaintext") ^= 0x01;
    sock.write_all(&record)?;
    sock.flush()
}
