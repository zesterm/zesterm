//! The client half of a daemon conversation, as a reusable object.
//!
//! Extracted from `remote.rs`'s private `Handshake` because attaching is no
//! longer the only conversation a client has: the tab strip lists sessions,
//! creates them on a chosen host, and closes them — none of which should have
//! to construct a `RemoteSession` to speak. `RemoteSession` builds on this
//! for its first connection and for every redial.
//!
//! The shape is synchronous and inline on purpose, exactly as the handshake
//! always was: a failure is an error the caller can fall back from, not a
//! window that opens and then reports it has nothing to show.

use std::io::{Read, Write};
use std::sync::Arc;

use zest_mesh::identity::{ClientIdentity, Nonce, Purpose, Signature};
use zest_mesh::pairing::{auth_transcript, verify_challenge, Transcript};
use zest_proto::{
    frame, ClientMessage, FrameReader, HostMessage, SessionAddr, SessionInfo, PROTOCOL_VERSION,
};

use crate::remote::RemoteError;

/// An authenticated connection to one daemon.
pub struct DaemonClient {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    frames: FrameReader,
    host: zest_proto::HostId,
    host_label: String,
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
    ) -> Result<Self, RemoteError> {
        let mut client = Self {
            read,
            write,
            frames: FrameReader::new(),
            host: zest_proto::HostId::from_bytes([0; 32]),
            host_label: String::new(),
        };

        let client_nonce = Nonce::random().map_err(|e| RemoteError::Io(e.to_string()))?;
        client.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: label.to_string(),
            nonce: zest_proto::Nonce32::from_bytes(*client_nonce.as_bytes()),
        })?;

        // Challenge -> Auth -> Welcome. Two round trips on connect, which on a
        // loopback socket is tens of microseconds and on a LAN is under a
        // millisecond -- paid once, against a session that lasts hours.
        loop {
            match client.recv()? {
                HostMessage::Challenge { version, host, label: host_label, nonce, signature } => {
                    if version != PROTOCOL_VERSION {
                        return Err(RemoteError::Version {
                            ours: PROTOCOL_VERSION,
                            theirs: version,
                        });
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
                    // dialled?
                    verify_challenge(expect_host, &transcript, &host_sig)
                        .map_err(|e| RemoteError::Refused(e.to_string()))?;

                    let sig = identity.sign(Purpose::Auth, &auth_transcript(&transcript));
                    client.send(&ClientMessage::Auth {
                        signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                    })?;
                }
                HostMessage::Welcome { version, host, label } => {
                    if version != PROTOCOL_VERSION {
                        return Err(RemoteError::Version {
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
                    return Err(RemoteError::Refused(format!("{reason:?}: {message}")));
                }
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
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
    pub fn list(&mut self) -> Result<Vec<SessionInfo>, RemoteError> {
        self.send(&ClientMessage::ListSessions)?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions } => return Ok(sessions),
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
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
    ) -> Result<SessionAddr, RemoteError> {
        self.send(&ClientMessage::CreateSession {
            command: command.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
        })?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions } => {
                    // The reply is the whole list; the newest entry is ours.
                    // Racy against a concurrent creator on the same host — the
                    // additive `created` field in the next protocol step is
                    // what retires this heuristic.
                    let Some(newest) = sessions.last() else { continue };
                    return Ok(newest.addr);
                }
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
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
    ) -> Result<SessionAddr, RemoteError> {
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
    ) -> Result<(u64, zest_proto::Keyframe), RemoteError> {
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
                        },
                    ));
                }
                HostMessage::Error { message, .. } => return Err(RemoteError::Refused(message)),
                _ => {}
            }
        }
    }

    /// End a session: the daemon hangs its child up and removes it.
    ///
    /// Fire-and-forget on the wire — the daemon sends no reply — so the only
    /// errors are transport ones.
    #[allow(dead_code, reason = "the fleet model is the second consumer, later in the #23 sequence")]
    pub fn close(&mut self, addr: SessionAddr) -> Result<(), RemoteError> {
        self.send(&ClientMessage::CloseSession { session: addr })
    }

    /// Surrender the transport, for a caller that now wants to stream.
    #[must_use]
    pub fn into_halves(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
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
}
