//! The client half of a daemon conversation, as a reusable object.
//!
//! Extracted from `remote.rs`'s private `Handshake` because attaching is no
//! longer the only conversation a client has: the tab strip lists sessions,
//! creates them on a chosen host, and closes them — none of which should have
//! to construct a `RemoteSession` to speak. `RemoteSession` builds on this
//! for its first connection and for every redial.
//!
//! **It lives here, in the daemon's own crate, rather than in the app.** It was
//! in `zest-app` while the app was the only client, and the daemon's tests and
//! its `attach`/`pair` examples each hand-rolled their own Hello → Challenge →
//! Auth. That was tolerable while a wrong peer failed loudly at the signature.
//! Protocol 3 made it untenable: the same steps now derive the key everything
//! afterwards is encrypted under, so a hand-rolled peer that gets the seal
//! switch or the transcript wrong produces a connection that authenticates
//! perfectly and then cannot be read — and nine copies is nine chances at it.
//! One implementation, exercised by the app *and* by every diagnostic.
//!
//! The shape is synchronous and inline on purpose, exactly as the handshake
//! always was: a failure is an error the caller can fall back from, not a
//! window that opens and then reports it has nothing to show.

use std::io::{Read, Write};
use std::sync::Arc;

use zest_mesh::identity::{ClientIdentity, Signature};
use zest_mesh::pairing::{Challenge, ClientHandshake};
use zest_mesh::secure::{DhPublic, SecureChannel};
use zest_proto::{
    frame, ClientMessage, FrameReader, HostMessage, SessionAddr, SessionInfo, PROTOCOL_VERSION,
};

use crate::DaemonError;

/// An authenticated connection to one daemon.
pub struct DaemonClient {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    frames: FrameReader,
    host: zest_proto::HostId,
    host_label: String,
    /// This connection's encryption, from the `Challenge` onwards.
    ///
    /// Two flags in one `Option`, as the daemon has: `Some(_)` means incoming
    /// frames are sealed, `sealing_out` means outgoing ones are. They flip at
    /// different moments because the switch is positional — the `Challenge`
    /// arrives in plaintext and the `Auth` that answers it is the first sealed
    /// frame going the other way.
    channel: Option<SecureChannel>,
    sealing_out: bool,
}

impl DaemonClient {
    /// Hello → Challenge → Auth → Welcome over an already-open transport.
    ///
    /// `expect_host` pins the far end: an address learned from an mDNS
    /// advertisement is a claim, and the host signing first is what lets the
    /// client hang up before revealing anything if the claim is false.
    /// `None` on loopback, where the socket's permissions are the answer.
    pub fn connect(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<zest_proto::HostId>,
        watch_sessions: bool,
    ) -> Result<Self, DaemonError> {
        let mut client = Self {
            read,
            write,
            frames: FrameReader::new(),
            host: zest_proto::HostId::from_bytes([0; 32]),
            host_label: String::new(),
            channel: None,
            sealing_out: false,
        };

        let mut hs = ClientHandshake::new(Arc::clone(identity), label)
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        client.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: label.to_string(),
            nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
            dh: zest_proto::Pub32::from_bytes(hs.dh().0),
            watch_sessions,
        })?;

        // Challenge -> Auth -> Welcome. Two round trips on connect, which on a
        // loopback socket is tens of microseconds and on a LAN is under a
        // millisecond -- paid once, against a session that lasts hours.
        loop {
            match client.recv()? {
                HostMessage::Challenge { version, host, label: host_label, nonce, dh, signature } => {
                    if version != PROTOCOL_VERSION {
                        return Err(DaemonError::Version {
                            ours: PROTOCOL_VERSION,
                            theirs: version,
                        });
                    }
                    let host_sig = Signature::from_slice(&signature.0)
                        .map_err(|e| DaemonError::Refused(e.to_string()))?;

                    // Verifies the host, signs, and derives the key, in that
                    // order and in one call. The ordering is the property —
                    // nothing is revealed and no key exists until the machine
                    // that answered has proved it is the one that was dialled —
                    // so it lives in `zest-mesh` where a test holds it, not
                    // here where it would be three steps a call site could
                    // reorder.
                    let (sig, _transcript, channel) = hs
                        .on_challenge(
                            expect_host,
                            &Challenge {
                                version,
                                host,
                                label: host_label,
                                nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                                dh: DhPublic(dh.0),
                                signature: host_sig,
                            },
                        )
                        .map_err(|e| DaemonError::Refused(e.to_string()))?;

                    // Incoming is sealed from the *next* frame; `send` flips
                    // outgoing after this `Auth`, which is itself sealed.
                    client.channel = Some(channel);
                    client.sealing_out = true;
                    client.send(&ClientMessage::Auth {
                        signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                    })?;
                }
                HostMessage::Welcome { version, host, label } => {
                    if version != PROTOCOL_VERSION {
                        return Err(DaemonError::Version {
                            ours: PROTOCOL_VERSION,
                            theirs: version,
                        });
                    }
                    // The Welcome's host id is what the fleet model keys on:
                    // it is how the app learns its own local daemon's identity
                    // without a wire change.
                    client.host = host;
                    client.host_label = label;
                    return Ok(client);
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
                    return Err(DaemonError::Refused(format!("{reason:?}: {message}")));
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                _ => {}
            }
        }
    }

    /// The machine on the far end, from its signed Welcome.
    #[must_use]
    #[allow(dead_code, reason = "the fleet model is the second consumer, later in the #23 sequence")]
    pub fn host(&self) -> zest_proto::HostId {
        self.host
    }

    #[must_use]
    pub fn host_label(&self) -> &str {
        &self.host_label
    }

    /// What sessions this host has.
    pub fn list(&mut self) -> Result<Vec<SessionInfo>, DaemonError> {
        self.send(&ClientMessage::ListSessions)?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions, .. } => return Ok(sessions),
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                _ => {}
            }
        }
    }

    /// Start a fresh session and return its address.
    pub fn create(
        &mut self,
        command: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> Result<SessionAddr, DaemonError> {
        self.send(&ClientMessage::CreateSession {
            command: command.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
        })?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions, created } => {
                    // The daemon names the created session outright. The
                    // `.last()` fallback survives only for an older daemon
                    // that predates the field — where it is racy against a
                    // concurrent creator, exactly as it always was.
                    if let Some(id) = created {
                        return Ok(SessionAddr::new(self.host, id));
                    }
                    let Some(newest) = sessions.last() else { continue };
                    return Ok(newest.addr);
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                _ => {}
            }
        }
    }

    /// Find a session to use, or make one.
    ///
    /// The adopt policy lives client-side on purpose: `ListSessions` then the
    /// newest unattached, else create. See `AttachOptions::adopt` for why
    /// adopting is the GUI default today.
    pub fn open_session(
        &mut self,
        command: &str,
        cols: u16,
        rows: u16,
        adopt: bool,
    ) -> Result<SessionAddr, DaemonError> {
        let existing = self.list()?;
        let adopted =
            adopt.then(|| existing.iter().rev().find(|s| !s.attached).map(|s| s.addr)).flatten();
        match adopted {
            Some(addr) => Ok(addr),
            None => self.create(command, "", cols, rows),
        }
    }

    /// Attach to a session that already exists, and take its keyframe.
    ///
    /// Every attach starts with a keyframe, including a reattach: the host has
    /// no idea what this client still holds, and after a link drop neither
    /// does the client with any confidence.
    pub fn attach(
        &mut self,
        addr: SessionAddr,
        cols: u16,
        rows: u16,
    ) -> Result<(u64, zest_proto::Keyframe), DaemonError> {
        self.send(&ClientMessage::Attach { session: addr, cols, rows })?;
        loop {
            match self.recv()? {
                HostMessage::Keyframe {
                    seq,
                    cols,
                    rows,
                    rows_data,
                    attrs,
                    cursor,
                    modes,
                    blocks,
                    title,
                    ..
                } => {
                    return Ok((
                        seq.0,
                        zest_proto::Keyframe {
                            cols,
                            rows,
                            rows_data,
                            attrs,
                            cursor,
                            modes: zest_core::Modes::from_bits_truncate(modes),
                            blocks,
                            title,
                        },
                    ));
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                _ => {}
            }
        }
    }

    /// End a session: the daemon hangs its child up and removes it.
    ///
    /// Fire-and-forget on the wire — the daemon sends no reply — so the only
    /// errors are transport ones.
    #[allow(dead_code, reason = "the fleet model is the second consumer, later in the #23 sequence")]
    pub fn close(&mut self, addr: SessionAddr) -> Result<(), DaemonError> {
        self.send(&ClientMessage::CloseSession { session: addr })
    }

    /// The next message from the host, blocking.
    ///
    /// For watchers: after `Hello.watch_sessions`, `Sessions` pushes arrive
    /// whenever the listing changes, and this is how they are read.
    pub fn next_message(&mut self) -> Result<HostMessage, DaemonError> {
        self.recv()
    }

    /// Surrender the transport, for a caller that now wants to stream.
    ///
    /// The channel goes with it, and must: the counters are per connection, so
    /// a streaming caller that started a fresh channel would seal under nonces
    /// the daemon has already seen. It is `Option` only because the field is —
    /// after a successful `connect` it is always present.
    #[must_use]
    pub fn into_halves(
        self,
    ) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, Option<SecureChannel>) {
        (self.read, self.write, self.channel)
    }

    fn send(&mut self, msg: &ClientMessage) -> Result<(), DaemonError> {
        let body = frame::encode_body(msg).map_err(|e| DaemonError::Transport(e.to_string()))?;
        let body = match (&mut self.channel, self.sealing_out) {
            (Some(ch), true) => ch.seal(&body).map_err(|e| DaemonError::Transport(e.to_string()))?,
            _ => body,
        };
        let bytes = frame::frame_bytes(&body).map_err(|e| DaemonError::Transport(e.to_string()))?;
        self.write.write_all(&bytes).map_err(|e| DaemonError::Transport(e.to_string()))?;
        self.write.flush().map_err(|e| DaemonError::Transport(e.to_string()))
    }

    fn recv(&mut self) -> Result<HostMessage, DaemonError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Ok(Some(body)) = self.frames.next_frame() {
                // A frame that will not open ends the connection rather than
                // being skipped: the counter has already advanced, so there is
                // no position to resume from.
                let body = match &mut self.channel {
                    Some(ch) => ch.open(&body).map_err(|e| DaemonError::Transport(e.to_string()))?,
                    None => body,
                };
                return frame::decode::<HostMessage>(&body)
                    .map_err(|e| DaemonError::Transport(e.to_string()));
            }
            let n = self.read.read(&mut buf).map_err(|e| DaemonError::Transport(e.to_string()))?;
            if n == 0 {
                return Err(DaemonError::Closed);
            }
            self.frames.feed(&buf[..n]);
        }
    }
}
