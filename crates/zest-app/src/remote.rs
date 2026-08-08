//! A session that lives in a daemon.
//!
//! Implements [`SessionSource`](crate::source::SessionSource) against
//! `zest-proto` instead of against a pty. The renderer cannot tell the
//! difference, because the grid it reads is a real `zest_core::Terminal` that
//! deltas are applied into — that is the promise `docs/CONTRACTS.md` makes, and
//! `RemoteWriter` is what keeps it.
//!
//! Deliberately shaped like [`crate::session::Session`], down to the thread
//! names and the `lock_unfair` + `yield_now` pair, so the two can be read side
//! by side and any divergence is visible rather than buried.
//!
//! # Detach is not exit
//!
//! A dropped connection means the *link* died. The shell is still running in a
//! daemon that does not care, which is the entire point of ADR-007 — close the
//! lid, pick the session up from a phone. Conflating the two would close the
//! window on every Wi-Fi hiccup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use zest_core::Terminal;
use zest_mesh::identity::{ClientIdentity, Nonce, Purpose, Signature};
use zest_mesh::pairing::{auth_transcript, verify_challenge, Transcript};
use zest_proto::{
    frame, Applied, Applier, ClientMessage, FrameReader, HostMessage, SessionAddr, Seq,
    PROTOCOL_VERSION,
};

use crate::fair_mutex::FairMutex;
use crate::session::Wakeup;
use crate::source::{Origin, SessionSource};

/// How often to acknowledge, at most.
///
/// One ack per applied delta would double the message count on a busy session
/// for information the host only needs approximately. A frame's worth of
/// coalescing costs nothing and matches how often the grid is looked at anyway.
const ACK_INTERVAL: Duration = Duration::from_millis(16);

/// What the writer thread does next.
///
/// No `Stream` variant, deliberately. Reconnecting would need one, and nothing
/// reconnects yet — a variant that exists for a path that does not is how an
/// enum accumulates arms nobody can explain.
enum Outbound {
    Msg(ClientMessage),
    Shutdown,
}

/// What to attach to, and how to describe this client while doing it.
#[derive(Debug, Clone, Copy)]
pub struct AttachOptions<'a> {
    /// The key this client proves it holds.
    ///
    /// Not a bare id: the host challenges, and only the holder of the secret
    /// can answer. On loopback the answer authorizes nothing -- the socket's
    /// permissions already did -- which is why an ephemeral key is enough there
    /// and the OS keychain stays off the startup path.
    pub identity: &'a ClientIdentity,
    /// Shown in the host's log and its approval prompt.
    pub label: &'a str,
    /// Empty means the host's default shell.
    pub command: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    /// Attach to an existing unattached session instead of starting a new one.
    ///
    /// **On by default**, and the reasoning reversed once the consequence was
    /// measured. Creating every time makes opening a terminal deterministic,
    /// which reads well -- and it means a session can never be picked up again,
    /// which is the entire promise of ADR-007, *and* it leaks: closing a window
    /// only detaches, so every launch left a shell running forever with no way
    /// to reach it.
    ///
    /// Adopting the newest unattached session is what "close the lid, pick it
    /// up later" actually means. `--new-session` forces a fresh one.
    pub adopt: bool,
    /// Whether this connection is the loopback socket.
    ///
    /// Decided by the transport rather than by comparing `HostId`s, because
    /// "did I reach this machine" and "did I reach the machine I dialled" are
    /// different questions and only the second is about identity.
    pub local: bool,
    /// The host this client believes it is dialling.
    ///
    /// Checked against the signed challenge, which is the whole reason the host
    /// signs first: an address learned from an mDNS advertisement is a claim,
    /// and without this "connect to my Mac" means "connect to whatever answered
    /// on that port". `None` on loopback, where the socket is the answer.
    pub expect_host: Option<zest_proto::HostId>,
}

pub struct RemoteSession {
    terminal: Arc<FairMutex<Terminal>>,
    /// Set by the reader, cleared by the renderer. Also the coalescing latch.
    needs_redraw: Arc<AtomicBool>,
    /// Interior mutability for `write`/`resize` on `&self` — the same trick
    /// `Session::pty_tx` uses, for the same reason.
    tx: Sender<Outbound>,
    addr: SessionAddr,
    origin: Origin,
    /// `cols << 16 | rows`, so a reconnect can replay the size as one read.
    /// Joined on drop, so the `Detach` is actually written before the process
    /// ends rather than racing it.
    writer: Option<std::thread::JoinHandle<()>>,
}

/// Detach when the client goes away.
///
/// **`Detach`, never `CloseSession`.** The shell keeps running in the daemon,
/// which is the entire payoff of ADR-007: close the lid, pick the session up
/// from a phone. A destructor rather than a `CloseRequested` arm because it
/// covers every way this process can end, including the ones no event handler
/// would see.
impl Drop for RemoteSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Outbound::Msg(ClientMessage::Detach { session: self.addr }));
        let _ = self.tx.send(Outbound::Shutdown);
        // Joined, not fire-and-forget: the writer runs on its own thread, and
        // without this the process can exit before the frame reaches the
        // socket -- leaving the daemon holding a subscriber for a client that
        // is already gone.
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

impl RemoteSession {
    /// Attach to a session on an already-connected daemon.
    ///
    /// `read`/`write` are the two halves of the connection; the handshake has
    /// not happened yet.
    pub fn attach(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        opts: &AttachOptions<'_>,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, RemoteError> {
        let &AttachOptions {
            identity,
            label,
            command,
            cols,
            rows,
            scrollback,
            adopt,
            local,
            expect_host,
        } = opts;
        let (tx, rx): (Sender<Outbound>, Receiver<Outbound>) = crossbeam_channel::unbounded();

        // The handshake runs inline, before any thread starts, so a failure is
        // an error the caller can fall back from rather than a window that
        // opens and then reports it has nothing to show.
        let mut conn = Handshake::new(read, write);
        let (host_label, addr, keyframe_seq, keyframe) =
            conn.run(identity, label, command, cols, rows, adopt, expect_host)?;
        let (mut reader, writer) = conn.into_halves();

        let terminal = Arc::new(FairMutex::new(Terminal::new(
            usize::from(cols),
            usize::from(rows),
            scrollback,
        )));
        let needs_redraw = Arc::new(AtomicBool::new(true));

        let mut applier = Applier::new();
        {
            let mut term = terminal.lock();
            applier.apply_keyframe(&mut term, &keyframe, keyframe_seq);
        }

        // --- writer thread ---
        let writer_thread = {
            let mut sink = writer;
            std::thread::Builder::new()
                .name("zest-remote-writer".into())
                .spawn(move || {
                    while let Ok(item) = rx.recv() {
                        match item {
                            Outbound::Msg(msg) => {
                                let Ok(bytes) = frame::encode(&msg) else { continue };
                                if sink.write_all(&bytes).is_err() || sink.flush().is_err() {
                                    // The reader is the supervisor and will
                                    // notice the same break; nothing to do here
                                    // but stop trying.
                                    continue;
                                }
                            }
                            Outbound::Shutdown => break,
                        }
                    }
                })
                .map_err(|e| RemoteError::Thread(e.to_string()))?
        };

        // --- reader thread ---
        {
            let terminal = Arc::clone(&terminal);
            let needs_redraw = Arc::clone(&needs_redraw);
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("zest-remote-reader".into())
                .spawn(move || {
                    let mut frames = FrameReader::new();
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut last_ack = Instant::now();
                    let mut pending_ack: Option<u64> = None;

                    loop {
                        let n = match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        frames.feed(&buf[..n]);

                        loop {
                            let body = match frames.next_frame() {
                                Ok(Some(b)) => b,
                                Ok(None) => break,
                                // Framing is lost, so the stream position is no
                                // longer trustworthy. Nothing to do but drop it.
                                Err(_) => return finish(&wake),
                            };

                            let Ok(msg) = frame::decode::<HostMessage>(&body) else {
                                // A message this build cannot parse is a newer
                                // host, not a broken one. Resync rather than
                                // disconnect -- this is what lets a future
                                // DeltaOp be added without a version bump.
                                let _ = tx.send(Outbound::Msg(ClientMessage::RequestKeyframe {
                                    session: addr,
                                }));
                                continue;
                            };

                            match msg {
                                HostMessage::Keyframe { seq, rows_data, attrs, cursor, cols, rows, modes, blocks, .. } => {
                                    let k = zest_proto::Keyframe {
                                        cols,
                                        rows,
                                        rows_data,
                                        attrs,
                                        cursor,
                                        modes: zest_core::Modes::from_bits_truncate(modes),
                                        blocks,
                                    };
                                    {
                                        let mut term = terminal.lock_unfair();
                                        applier.apply_keyframe(&mut term, &k, seq.0);
                                    }
                                    pending_ack = Some(seq.0);
                                    mark(&needs_redraw, &wake);
                                }
                                HostMessage::Update { base, seq, delta, .. } => {
                                    let outcome = {
                                        let mut term = terminal.lock_unfair();
                                        applier.apply_delta(&mut term, &delta, base.0, seq.0)
                                    };
                                    match outcome {
                                        Applied::Ok => {
                                            pending_ack = Some(seq.0);
                                            mark(&needs_redraw, &wake);
                                        }
                                        // Nothing was applied. Ask for a whole
                                        // state rather than carrying on against
                                        // a grid that has silently diverged.
                                        Applied::NeedsKeyframe => {
                                            let _ = tx.send(Outbound::Msg(
                                                ClientMessage::RequestKeyframe { session: addr },
                                            ));
                                        }
                                    }
                                }
                                HostMessage::Scrollback { rows_data, attrs, .. } => {
                                    let mut term = terminal.lock_unfair();
                                    applier.absorb_attrs(&attrs);
                                    applier.apply_scrollback(&mut term, &rows_data);
                                    drop(term);
                                    mark(&needs_redraw, &wake);
                                }
                                // Returns rather than breaking, so `finish`
                                // below is only ever reached when the *link*
                                // died -- which is what makes the distinction
                                // between Exited and Detached structural.
                                HostMessage::Exited { .. } => {
                                    wake(Wakeup::Exited);
                                    return;
                                }
                                HostMessage::Error { message, .. } => {
                                    tracing::warn!(%message, "daemon reported an error");
                                }
                                _ => {}
                            }
                        }

                        // Coalesced, and always after a keyframe so the host
                        // learns the resync landed.
                        if let Some(seq) = pending_ack {
                            if last_ack.elapsed() >= ACK_INTERVAL {
                                let _ = tx.send(Outbound::Msg(ClientMessage::Ack {
                                    session: addr,
                                    seq: Seq(seq),
                                }));
                                pending_ack = None;
                                last_ack = Instant::now();
                            }
                        }

                        // The reader is the thread in the tight loop, so it
                        // must not barge ahead of a renderer already waiting.
                        std::thread::yield_now();
                    }

                    finish(&wake);
                })
                .map_err(|e| RemoteError::Thread(e.to_string()))?;
        }

        Ok(Self {
            terminal,
            needs_redraw,
            tx,
            addr,
            origin: Origin::Daemon { host: host_label, local },
            writer: Some(writer_thread),
        })
    }


}

fn mark(needs_redraw: &Arc<AtomicBool>, wake: &impl Fn(Wakeup)) {
    // Only on the false->true transition, so a 100MB `cat` posts one event per
    // frame rather than millions. Same latch as the local session.
    if !needs_redraw.swap(true, Ordering::AcqRel) {
        wake(Wakeup::Redraw);
    }
}

/// The read loop ended without the child having exited.
///
/// Only reachable when the *link* died: the `Exited` arm returns rather than
/// breaking, so reaching here means the connection went away while the shell
/// kept running. `Detached`, never `Exited`.
fn finish(wake: &impl Fn(Wakeup)) {
    wake(Wakeup::Detached);
}

impl SessionSource for RemoteSession {
    fn terminal(&self) -> &Arc<FairMutex<Terminal>> {
        &self.terminal
    }

    fn write(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let _ = self
            .tx
            .send(Outbound::Msg(ClientMessage::Input { session: self.addr, bytes }));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self
            .tx
            .send(Outbound::Msg(ClientMessage::Resize { session: self.addr, cols, rows }));

        // And ask for the whole state back.
        //
        // Nothing else would tell this client the grid changed shape. There is
        // no `DeltaOp::Resize`, and the size only ever travels in a keyframe --
        // so on a *shrink* the deltas that follow describe rows 0..new_rows,
        // every one of which lands inside the client's older, larger grid. No
        // row falls past the end, `Applied::NeedsKeyframe` never fires, and the
        // client keeps a grid the host no longer has: the rows below the new
        // height stay on screen holding whatever they last held, and everything
        // above is misaligned against a viewport that has moved.
        //
        // Growing happened to work by accident, because a taller grid does push
        // rows past the end and trips the resync. That asymmetry is exactly why
        // this is explicit rather than left to the applier.
        //
        // The host's answer is authoritative, which matters: another client may
        // be attached at a different size, so what this one asked for is a
        // request, not a fact. Resizing is human-speed, so a whole state costs
        // nothing worth counting.
        let _ = self
            .tx
            .send(Outbound::Msg(ClientMessage::RequestKeyframe { session: self.addr }));
    }

    fn take_dirty(&self) -> bool {
        self.needs_redraw.swap(false, Ordering::AcqRel)
    }

    fn mark_dirty(&self) {
        self.needs_redraw.store(true, Ordering::Release);
    }

    fn origin(&self) -> Origin {
        self.origin.clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("daemon speaks protocol {theirs}, this build speaks {ours}")]
    Version { ours: u16, theirs: u16 },
    #[error("the daemon refused this client: {0}")]
    Refused(String),
    #[error("the daemon closed the connection during the handshake")]
    Closed,
    #[error("transport failed: {0}")]
    Io(String),
    #[error("could not start a thread: {0}")]
    Thread(String),
}

/// Hello → Welcome → CreateSession → Attach → Keyframe, inline.
struct Handshake {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    frames: FrameReader,
}

impl Handshake {
    fn new(read: Box<dyn Read + Send>, write: Box<dyn Write + Send>) -> Self {
        Self { read, write, frames: FrameReader::new() }
    }

    fn into_halves(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
        (self.read, self.write)
    }

    fn send(&mut self, msg: &ClientMessage) -> Result<(), RemoteError> {
        let bytes = frame::encode(msg).map_err(|e| RemoteError::Io(e.to_string()))?;
        self.write.write_all(&bytes).map_err(|e| RemoteError::Io(e.to_string()))?;
        self.write.flush().map_err(|e| RemoteError::Io(e.to_string()))
    }

    fn recv(&mut self) -> Result<HostMessage, RemoteError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Ok(Some(body)) = self.frames.next_frame() {
                return frame::decode::<HostMessage>(&body)
                    .map_err(|e| RemoteError::Io(e.to_string()));
            }
            let n = self.read.read(&mut buf).map_err(|e| RemoteError::Io(e.to_string()))?;
            if n == 0 {
                return Err(RemoteError::Closed);
            }
            self.frames.feed(&buf[..n]);
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one handshake, one call site")]
    fn run(
        &mut self,
        identity: &ClientIdentity,
        label: &str,
        command: &str,
        cols: u16,
        rows: u16,
        adopt: bool,
        expect_host: Option<zest_proto::HostId>,
    ) -> Result<(String, SessionAddr, u64, zest_proto::Keyframe), RemoteError> {
        let client_nonce = Nonce::random().map_err(|e| RemoteError::Io(e.to_string()))?;
        self.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: label.to_string(),
            nonce: zest_proto::Nonce32::from_bytes(*client_nonce.as_bytes()),
        })?;

        // Challenge -> Auth -> Welcome. Two round trips on connect, which on a
        // loopback socket is tens of microseconds and on a LAN is under a
        // millisecond -- paid once, against a session that lasts hours.
        let host_label = loop {
            match self.recv()? {
                HostMessage::Challenge { version, host, label: host_label, nonce, signature } => {
                    if version != PROTOCOL_VERSION {
                        return Err(RemoteError::Version { ours: PROTOCOL_VERSION, theirs: version });
                    }
                    let transcript = Transcript {
                        version,
                        host,
                        client: identity.client_id(),
                        host_nonce: Nonce::from_bytes(nonce.0),
                        client_nonce,
                        host_label: host_label.clone(),
                        client_label: label.to_string(),
                    };
                    let host_sig = Signature::from_slice(&signature.0)
                        .map_err(|e| RemoteError::Refused(e.to_string()))?;

                    // Before revealing anything: is this the machine that was
                    // dialled? On a LAN the address came from an advertisement,
                    // which is a claim rather than a fact.
                    verify_challenge(expect_host, &transcript, &host_sig)
                        .map_err(|e| RemoteError::Refused(e.to_string()))?;

                    let sig = identity.sign(Purpose::Auth, &auth_transcript(&transcript));
                    self.send(&ClientMessage::Auth {
                        signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                    })?;
                }
                HostMessage::Welcome { version, label, .. } => {
                    if version != PROTOCOL_VERSION {
                        return Err(RemoteError::Version { ours: PROTOCOL_VERSION, theirs: version });
                    }
                    break label;
                }
                // Not an error: the key is good and nobody has said yes yet.
                // The caller decides how long to wait.
                HostMessage::AuthPending { code, expires_in_secs } => {
                    tracing::info!(
                        %code,
                        expires_in_secs,
                        "waiting for this device to be approved on the host"
                    );
                }
                HostMessage::AuthFailed { reason, message } => {
                    return Err(RemoteError::Refused(format!("{reason:?}: {message}")));
                }
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
                _ => {}
            }
        };

        // Ask what is running first. The listing costs one round trip inside
        // the window GPU initialization is already occupying, and it is what
        // makes adopting possible at all.
        self.send(&ClientMessage::ListSessions)?;
        let existing = loop {
            match self.recv()? {
                HostMessage::Sessions { sessions } => break sessions,
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
                _ => {}
            }
        };

        // Default is create, not adopt: silently reattaching to whatever
        // happened to be there makes opening a window nondeterministic.
        let adopted = adopt
            .then(|| existing.iter().rev().find(|s| !s.attached).map(|s| s.addr))
            .flatten();

        let addr = match adopted {
            Some(addr) => addr,
            None => {
                self.send(&ClientMessage::CreateSession {
                    command: command.to_string(),
                    cwd: String::new(),
                    cols,
                    rows,
                })?;
                loop {
                    match self.recv()? {
                        HostMessage::Sessions { sessions } => {
                            let Some(newest) = sessions.last() else { continue };
                            break newest.addr;
                        }
                        HostMessage::Error { message, .. } => {
                            return Err(RemoteError::Refused(message))
                        }
                        _ => {}
                    }
                }
            }
        };

        self.send(&ClientMessage::Attach { session: addr, cols, rows })?;

        loop {
            match self.recv()? {
                HostMessage::Keyframe { seq, cols, rows, rows_data, attrs, cursor, modes, blocks, .. } => {
                    return Ok((
                        host_label,
                        addr,
                        seq.0,
                        zest_proto::Keyframe {
                            cols,
                            rows,
                            rows_data,
                            attrs,
                            cursor,
                            modes: zest_core::Modes::from_bits_truncate(modes),
                            blocks,
                        },
                    ));
                }
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
                _ => {}
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A daemon on a real socket, in this process.
    ///
    /// Deliberately a real socket rather than an in-memory pipe: the whole
    /// point of this type is that it speaks the wire protocol over a transport,
    /// and a test that skipped the transport would not exercise framing.
    struct Harness {
        path: String,
        registry: Arc<zest_daemon::Registry>,
    }

    impl Harness {
        fn start(name: &str) -> Self {
            // Short path: a unix socket must fit in SUN_LEN (~104 bytes).
            let path = format!("/tmp/zr-{}-{}.sock", name, std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{path}.lock"));

            let registry = Arc::new(zest_daemon::Registry::new());
            // A real host key, because the handshake is real: these tests drive
            // the same code the daemon binary does.
            let auth = Arc::new(zest_daemon::Authenticator::new(
                Arc::new(zest_mesh::identity::HostIdentity::generate().expect("host key")),
                Arc::new(zest_mesh::trust::MemoryTrustStore::new()),
                zest_mesh::pairing::PairingQueue::new(),
                "harness",
            ));
            let cfg = zest_daemon::DaemonConfig {
                host: zest_proto::HostId::from_bytes([3; 32]),
                label: "harness".into(),
                local_socket: path.clone(),
                listen_lan: false,
                lan_bind: "127.0.0.1".into(),
                lan_port: 0,
                shell_integration: true,
            };
            {
                let registry = Arc::clone(&registry);
                let path = path.clone();
                std::thread::spawn(move || {
                    let _ = zest_daemon::listen(&path, cfg, registry, auth);
                });
            }

            let deadline = Instant::now() + Duration::from_secs(5);
            while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Self { path, registry }
        }

        fn attach(&self, command: &str, wake: impl Fn(Wakeup) + Send + 'static) -> RemoteSession {
            self.attach_with(command, false, wake)
        }

        fn attach_with(
            &self,
            command: &str,
            adopt: bool,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            let stream = zest_daemon::connect(&self.path).expect("connect");
            let write = stream.try_clone().expect("clone");
            let identity = ClientIdentity::generate().expect("client key");
            RemoteSession::attach(
                Box::new(stream),
                Box::new(write),
                &AttachOptions {
                    identity: &identity,
                    label: "test",
                    command,
                    cols: 40,
                    rows: 6,
                    scrollback: 100,
                    adopt,
                    local: true,
                    expect_host: None,
                },
                wake,
            )
            .expect("attach")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}.lock", self.path));
        }
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn output_from_a_daemon_session_reaches_the_local_grid() {
        // The whole client half in one assertion: a shell running in another
        // process, its output encoded as deltas, applied into a Terminal here,
        // and readable through exactly the path the renderer uses.
        let h = Harness::start("output");
        let s = h.attach("/bin/echo hello-from-the-daemon", |_| {});

        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("hello-from-the-daemon")),
            "output never arrived; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn the_dirty_latch_coalesces_and_clears() {
        // The 0%-idle guarantee, on the remote path. A wake per delta rather
        // than per transition would post an event per frame forever.
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);

        let h = Harness::start("latch");
        let s = h.attach("/bin/echo latched", move |w| {
            if w == Wakeup::Redraw {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });

        assert!(wait_for(|| s.take_dirty()), "never went dirty");
        assert!(!s.take_dirty(), "the latch did not clear");

        // Whatever arrives, the wake count must stay far below the number of
        // deltas -- it is a transition counter, not a message counter.
        let seen = woken.load(Ordering::Relaxed);
        assert!(seen <= 4, "woke {seen} times for one echo; the latch is not coalescing");
    }

    #[test]
    fn typing_reaches_the_child_and_its_echo_comes_back() {
        let h = Harness::start("typing");
        let s = h.attach("/bin/sh", |_| {});

        // Wait for the shell to be up before typing, or the input races the
        // pty being ready and is swallowed.
        std::thread::sleep(Duration::from_millis(300));
        s.write(b"echo round-trip\n".to_vec());

        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("round-trip")),
            "the echo never came back; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn shrinking_the_viewport_resizes_the_local_grid_too() {
        // The size only ever travels in a keyframe -- there is no
        // DeltaOp::Resize -- so on a shrink every following row lands inside
        // the client's older, larger grid, no row falls past the end, and
        // `NeedsKeyframe` never fires. Without an explicit request the client
        // keeps a grid the host no longer has.
        //
        // Growing happened to work by accident, because a taller grid does
        // push rows past the end. That asymmetry is why this is tested from
        // the shrinking side.
        let h = Harness::start("shrink");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(s.terminal().lock().grid().rows(), 6, "attached at the wrong size");

        s.resize(20, 3);

        assert!(
            wait_for(|| {
                let t = s.terminal().lock();
                t.grid().rows() == 3 && t.grid().cols() == 20
            }),
            "the client kept a {}x{} grid after asking for 20x3",
            s.terminal().lock().grid().cols(),
            s.terminal().lock().grid().rows()
        );
    }

    #[test]
    fn reopening_picks_up_the_session_instead_of_leaking_it() {
        // Closing a window only detaches -- the shell keeps running, which is
        // the point. But creating a *new* session on every launch meant the
        // old one could never be reached again: one orphaned shell and one
        // orphaned pty per launch, growing until reboot, with nothing in the
        // UI or the CLI able to list or close them.
        let h = Harness::start("adopt");

        let first = h.attach_with("/bin/sh", true, |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.registry.len(), 1);
        drop(first);
        std::thread::sleep(Duration::from_millis(200));

        // The session survived, and reopening finds it rather than adding one.
        let _second = h.attach_with("/bin/sh", true, |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            h.registry.len(),
            1,
            "reopening created a second session; the first is now unreachable"
        );
    }

    #[test]
    fn a_session_still_updates_after_a_resize() {
        // The bug this exists for: `RequestKeyframe` replied with `Seq(0)`, so
        // the client's baseline went to 0 while the daemon's stayed at the real
        // sequence. Every following update was then refused as stale and
        // triggered another keyframe that again said 0 -- and because a resize
        // is what sends RequestKeyframe, resizing froze the terminal.
        //
        // Asserting that output arrives *after* a resize is what catches it;
        // asserting the resize itself does not.
        let h = Harness::start("afterresize");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));

        s.resize(30, 5);
        assert!(
            wait_for(|| {
                let t = s.terminal().lock();
                t.grid().rows() == 5 && t.grid().cols() == 30
            }),
            "the resize never took"
        );

        // The session must still be live afterwards.
        s.write(b"echo after-resize-works\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("after-resize-works")),
            "nothing arrived after a resize.\ngrid:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn closing_the_client_leaves_the_session_running() {
        // ADR-007's whole payoff, from the client side: close the window, the
        // shell keeps running, and it can be picked up again. Dropping the
        // session is what a closed window does.
        let h = Harness::start("outlive");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(h.registry.len(), 1, "the session was never created");
        drop(s);
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(
            h.registry.len(),
            1,
            "closing a client ended the session -- a shell must outlive the window"
        );
    }

    #[test]
    fn a_lost_connection_is_detached_and_never_exited() {
        // Conflating the two would close the window on every Wi-Fi hiccup,
        // throwing away a session that is still running.
        let seen: Arc<std::sync::Mutex<Vec<Wakeup>>> = Arc::default();
        let sink = Arc::clone(&seen);

        let h = Harness::start("detach");
        // A command that ends immediately would report Exited legitimately, so
        // this uses a shell that stays up and has its connection cut instead.
        let s = h.attach("/bin/sh", move |w| {
            sink.lock().expect("lock").push(w);
        });
        std::thread::sleep(Duration::from_millis(300));

        // Drop the session: the writer sends Detach and Shutdown, and the
        // daemon closes the connection, which ends the reader's loop.
        drop(s);
        std::thread::sleep(Duration::from_millis(400));

        let events = seen.lock().expect("lock");
        assert!(
            !events.contains(&Wakeup::Exited),
            "a dropped connection reported Exited: {events:?}"
        );
    }
}
