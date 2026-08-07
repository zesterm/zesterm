//! Answering a client.
//!
//! Deliberately written against `Read + Write` rather than against a socket, so
//! the whole protocol loop can be driven from a byte buffer in a test. A message
//! handler that can only be exercised through a real connection is one whose
//! error paths are never exercised at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use zest_proto::{
    frame, ClientMessage, FrameReader, HostMessage, HostId, SessionAddr, SessionId, SessionInfo,
    Seq, PROTOCOL_VERSION,
};
use zest_pty::{CommandSpec, PtySize};

use crate::session::{Session, Update};
use crate::{DaemonConfig, DaemonError};

/// Every session this machine owns.
///
/// Shared between connections, because the point of the daemon is that a session
/// is not owned by whoever happens to be looking at it.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next_id: Mutex<u64>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a session and keep it.
    pub fn create(
        &self,
        cmd: &CommandSpec,
        size: PtySize,
        scrollback: usize,
    ) -> Result<Arc<Session>, DaemonError> {
        let id = {
            let mut next = self.next_id.lock().expect("id lock");
            *next += 1;
            SessionId(*next)
        };
        let session = Arc::new(Session::spawn(id, cmd, size, scrollback, |_| {})?);
        self.sessions.lock().expect("registry lock").insert(id.0, Arc::clone(&session));
        Ok(session)
    }

    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().expect("registry lock").get(&id.0).cloned()
    }

    /// Everything running, for a listing.
    #[must_use]
    pub fn list(&self, host: HostId) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().expect("registry lock");
        let mut out: Vec<SessionInfo> = sessions
            .values()
            .map(|s| {
                let (cols, rows) = s.size();
                SessionInfo {
                    addr: SessionAddr::new(host, s.id),
                    title: s.title(),
                    cwd: String::new(),
                    cols,
                    rows,
                    alt_screen: s.alt_screen(),
                    attached: s.attached(),
                }
            })
            .collect();
        // Sorted so a fleet listing does not reshuffle between polls, which
        // makes a list on a phone unusable.
        out.sort_by_key(|s| s.addr.session.0);
        out
    }

    /// Drop a session and end its child.
    pub fn close(&self, id: SessionId) {
        self.sessions.lock().expect("registry lock").remove(&id.0);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("registry lock").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One client connection.
pub struct Connection {
    config: DaemonConfig,
    registry: Arc<Registry>,
    reader: FrameReader,
    /// Session id to this connection's subscriber handle.
    ///
    /// Per connection, not per session: two devices watching one shell each need
    /// their own position in it.
    attached: HashMap<u64, u64>,
    greeted: bool,
    /// Handed to each session on attach, so output wakes the writer.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Connection {
    #[must_use]
    pub fn new(config: DaemonConfig, registry: Arc<Registry>) -> Self {
        Self {
            config,
            registry,
            reader: FrameReader::new(),
            attached: HashMap::new(),
            greeted: false,
            waker: None,
        }
    }

    /// Set what an attached session calls when it has output.
    pub fn set_waker(&mut self, waker: Box<dyn Fn() + Send + Sync>) {
        self.waker = Some(Arc::from(waker));
    }

    /// Feed bytes from the client and collect what to send back.
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Result<Vec<HostMessage>, DaemonError> {
        self.reader.feed(bytes);
        let mut out = Vec::new();
        loop {
            let body = match self.reader.next_frame() {
                Ok(Some(b)) => b,
                Ok(None) => break,
                // A framing error means the stream position is no longer
                // trustworthy, so the caller must drop the connection rather
                // than try to continue reading past it.
                Err(e) => return Err(DaemonError::Transport(e.to_string())),
            };

            match frame::decode::<ClientMessage>(&body) {
                Ok(msg) => out.extend(self.handle(msg)),
                // A message we cannot parse is *not* fatal: a newer client may
                // send something this build has never heard of, and dropping the
                // connection over it would make every upgrade a hard cutover.
                Err(e) => {
                    tracing::warn!(error = %e, "unparseable message; ignoring");
                    out.push(HostMessage::Error {
                        session: None,
                        message: format!("could not understand that message: {e}"),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Anything the attached sessions have produced since the last call.
    pub fn poll(&mut self) -> Vec<HostMessage> {
        let mut out = Vec::new();
        for (&id, &handle) in &self.attached {
            let Some(session) = self.registry.get(SessionId(id)) else { continue };
            let addr = SessionAddr::new(self.config.host, SessionId(id));

            match session.poll(handle) {
                Some(Update::Delta(delta)) => out.push(HostMessage::Update {
                    session: addr,
                    base: Seq(0),
                    seq: Seq(0),
                    delta,
                }),
                Some(Update::Keyframe(k)) => out.push(HostMessage::Keyframe {
                    session: addr,
                    seq: Seq(0),
                    cols: k.cols,
                    rows: k.rows,
                    rows_data: k.rows_data,
                    attrs: k.attrs,
                    cursor: k.cursor,
                }),
                None => {}
            }

            if session.has_exited() {
                out.push(HostMessage::Exited { session: addr, code: None });
            }
        }
        out
    }

    fn handle(&mut self, msg: ClientMessage) -> Vec<HostMessage> {
        // `Hello` first, always. Serving a client whose protocol version has not
        // been checked means a mismatch shows up as corrupt state rather than as
        // a clear refusal.
        if !self.greeted && !matches!(msg, ClientMessage::Hello { .. }) {
            return vec![HostMessage::Error {
                session: None,
                message: "expected Hello first".into(),
            }];
        }

        match msg {
            ClientMessage::Hello { version, client, label } => {
                self.greeted = true;
                if version != PROTOCOL_VERSION {
                    // Refused, and said so. A version mismatch that silently
                    // proceeds produces a corrupt grid on the client and looks
                    // like a rendering bug.
                    return vec![HostMessage::Error {
                        session: None,
                        message: format!(
                            "protocol {version} is not compatible with {PROTOCOL_VERSION}"
                        ),
                    }];
                }
                tracing::info!(client = %client.short(), %label, "client connected");
                vec![HostMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    host: self.config.host,
                    label: self.config.label.clone(),
                }]
            }

            ClientMessage::ListSessions => {
                vec![HostMessage::Sessions { sessions: self.registry.list(self.config.host) }]
            }

            ClientMessage::CreateSession { command, cwd, cols, rows } => {
                let mut spec = CommandSpec::default_shell();
                if !command.is_empty() {
                    spec.command_line = command;
                }
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd.into());
                }
                match self.registry.create(&spec, PtySize::new(cols, rows), 10_000) {
                    Ok(_) => {
                        vec![HostMessage::Sessions {
                            sessions: self.registry.list(self.config.host),
                        }]
                    }
                    Err(e) => vec![HostMessage::Error {
                        session: None,
                        message: format!("could not start a session: {e}"),
                    }],
                }
            }

            ClientMessage::Attach { session, cols, rows } => {
                let Some(s) = self.registry.get(session.session) else {
                    return vec![Self::no_such(session)];
                };
                s.resize(cols, rows);
                let waker = self.waker.clone();
                let (handle, keyframe) = s.attach_with(Box::new(move || {
                    if let Some(w) = &waker {
                        w();
                    }
                }));
                self.attached.insert(session.session.0, handle);
                vec![HostMessage::Keyframe {
                    session,
                    seq: Seq(0),
                    cols: keyframe.cols,
                    rows: keyframe.rows,
                    rows_data: keyframe.rows_data,
                    attrs: keyframe.attrs,
                    cursor: keyframe.cursor,
                }]
            }

            ClientMessage::Detach { session } => {
                if let (Some(s), Some(handle)) =
                    (self.registry.get(session.session), self.attached.remove(&session.session.0))
                {
                    // Removes the subscriber and nothing else. The shell keeps
                    // running -- that is the whole design. → ADR-007.
                    s.detach(handle);
                }
                Vec::new()
            }

            ClientMessage::Input { session, bytes } => {
                match self.registry.get(session.session) {
                    Some(s) => {
                        s.write(&bytes);
                        Vec::new()
                    }
                    None => vec![Self::no_such(session)],
                }
            }

            ClientMessage::Resize { session, cols, rows } => {
                if let Some(s) = self.registry.get(session.session) {
                    s.resize(cols, rows);
                }
                Vec::new()
            }

            // Acknowledgement is tracked inside the session's subscriber, which
            // advances on every successful poll. Kept in the protocol because a
            // lossy transport will need it; a no-op over a reliable stream.
            ClientMessage::Ack { .. } => Vec::new(),

            ClientMessage::RequestScrollback { session, from_line, .. } => {
                vec![HostMessage::Scrollback { session, from_line, rows_data: Vec::new() }]
            }

            ClientMessage::CloseSession { session } => {
                self.attached.remove(&session.session.0);
                self.registry.close(session.session);
                Vec::new()
            }
        }
    }

    fn no_such(session: SessionAddr) -> HostMessage {
        HostMessage::Error {
            session: Some(session),
            message: format!("no session {session}"),
        }
    }
}

/// Serve one connection until it closes.
///
/// Takes the read and write halves **separately**, and that is the whole design
/// rather than an inconvenience. A single stream behind a mutex deadlocks: the
/// reader holds the lock across a blocking `read`, which is exactly what a
/// server should be doing while a client is quiet, and the writer can then never
/// acquire it to push what a session produced.
///
/// Polling on a timer would avoid the split and cost the 0%-idle guarantee — a
/// daemon that wakes ten times a second to find nothing is a laptop that does
/// not sleep.
pub fn serve<R, W>(
    mut reader: R,
    mut writer: W,
    config: DaemonConfig,
    registry: Arc<Registry>,
) -> Result<(), DaemonError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let conn = Arc::new(Mutex::new(Connection::new(config, Arc::clone(&registry))));
    let (tx, rx) = std::sync::mpsc::channel::<Wake>();

    {
        let conn = Arc::clone(&conn);
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("zest-daemon-conn-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let outgoing = {
                        let mut c = conn.lock().expect("connection lock");
                        match c.on_bytes(&buf[..n]) {
                            Ok(msgs) => msgs,
                            // Framing is broken, so the stream position can no
                            // longer be trusted and reading on would produce
                            // garbage. Ending the connection is the only honest
                            // response.
                            Err(e) => {
                                tracing::warn!(error = %e, "framing lost; closing");
                                break;
                            }
                        }
                    };
                    if tx.send(Wake::Send(outgoing)).is_err() {
                        break;
                    }
                }
                let _ = tx.send(Wake::Closed);
            })
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
    }

    // Registered on every attach, so a session wakes the writer directly rather
    // than the writer discovering output by looking for it.
    {
        let tx = tx.clone();
        conn.lock().expect("connection lock").set_waker(Box::new(move || {
            let _ = tx.send(Wake::Poll);
        }));
    }

    while let Ok(wake) = rx.recv() {
        let mut outgoing = match wake {
            Wake::Closed => return Ok(()),
            Wake::Send(msgs) => msgs,
            Wake::Poll => Vec::new(),
        };
        outgoing.extend(conn.lock().expect("connection lock").poll());
        if outgoing.is_empty() {
            continue;
        }

        for msg in outgoing {
            let bytes = frame::encode(&msg).map_err(|e| DaemonError::Transport(e.to_string()))?;
            // Logged, not swallowed. A write failure treated as a clean
            // disconnect is indistinguishable from a client that left, and the
            // difference is the whole diagnosis when nothing arrives.
            if let Err(e) = writer.write_all(&bytes) {
                tracing::debug!(error = %e, "write failed; client is gone");
                return Ok(());
            }
        }
        if let Err(e) = writer.flush() {
            tracing::debug!(error = %e, "flush failed; client is gone");
            return Ok(());
        }
    }
    Ok(())
}

/// Why the writer woke.
enum Wake {
    /// The reader produced replies.
    Send(Vec<HostMessage>),
    /// A session has output waiting.
    Poll,
    /// The client went away.
    Closed,
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use zest_proto::ClientId;

    fn config() -> DaemonConfig {
        DaemonConfig {
            host: HostId::from_bytes([5; 32]),
            label: "test-host".into(),
            local_socket: String::new(),
            listen_lan: false,
        }
    }

    fn conn() -> (Connection, Arc<Registry>) {
        let registry = Arc::new(Registry::new());
        (Connection::new(config(), Arc::clone(&registry)), registry)
    }

    fn send(c: &mut Connection, msg: &ClientMessage) -> Vec<HostMessage> {
        let bytes = frame::encode(msg).expect("encode");
        c.on_bytes(&bytes).expect("handled")
    }

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([1; 32]),
            label: "test".into(),
        }
    }

    fn echo_cmd() -> String {
        if cfg!(windows) { "cmd.exe /c echo probe".into() } else { "/bin/echo probe".into() }
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
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
    fn hello_is_answered_with_this_hosts_identity() {
        let (mut c, _) = conn();
        let out = send(&mut c, &hello());
        assert!(matches!(&out[..], [HostMessage::Welcome { host, .. }] if *host == config().host));
    }

    #[test]
    fn a_version_mismatch_is_refused_rather_than_served() {
        // Proceeding anyway produces a corrupt grid on the client, which looks
        // like a rendering bug and gets chased in the wrong codebase.
        let (mut c, _) = conn();
        let out = send(
            &mut c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION + 99,
                client: ClientId::from_bytes([1; 32]),
                label: "future".into(),
            },
        );
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn nothing_is_served_before_hello() {
        let (mut c, _) = conn();
        let out = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn an_unparseable_message_does_not_drop_the_connection() {
        // A newer client may send something this build has never heard of.
        // Dropping the connection would make every upgrade a hard cutover.
        let (mut c, _) = conn();
        send(&mut c, &hello());

        let mut junk = Vec::new();
        junk.extend_from_slice(&(4u32).to_le_bytes());
        junk.extend_from_slice(b"junk");
        let out = c.on_bytes(&junk).expect("not fatal");
        assert!(matches!(&out[..], [HostMessage::Error { .. }]));

        // ...and the connection still works.
        let after = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&after[..], [HostMessage::Sessions { .. }]), "{after:?}");
    }

    #[test]
    fn creating_a_session_lists_it() {
        let (mut c, registry) = conn();
        send(&mut c, &hello());

        let out = send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        assert!(matches!(&out[..], [HostMessage::Sessions { sessions }] if sessions.len() == 1));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn attaching_returns_a_keyframe_and_then_output() {
        let (mut c, registry) = conn();
        send(&mut c, &hello());
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        let out = send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
        assert!(matches!(&out[..], [HostMessage::Keyframe { .. }]), "{out:?}");

        assert!(
            wait_for(|| !c.poll().is_empty()),
            "the child produced output but nothing reached the client"
        );
    }

    #[test]
    fn detaching_leaves_the_session_in_the_registry() {
        // The property ADR-007 exists for: a client leaving must not end a shell.
        let (mut c, registry) = conn();
        send(&mut c, &hello());
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        send(&mut c, &ClientMessage::Detach { session: addr });
        assert_eq!(registry.len(), 1, "detaching removed the session");
        assert!(registry.get(addr.session).is_some());
    }

    #[test]
    fn closing_a_session_removes_it() {
        let (mut c, registry) = conn();
        send(&mut c, &hello());
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        send(&mut c, &ClientMessage::CloseSession { session: addr });
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn input_for_an_unknown_session_is_an_error_not_a_panic() {
        let (mut c, _) = conn();
        send(&mut c, &hello());
        let addr = SessionAddr::new(config().host, SessionId(999));
        let out = send(&mut c, &ClientMessage::Input { session: addr, bytes: vec![b'x'] });
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn a_listing_keeps_a_stable_order() {
        // A list that reshuffles between polls is unusable on a phone.
        let (mut c, registry) = conn();
        send(&mut c, &hello());
        for _ in 0..4 {
            send(
                &mut c,
                &ClientMessage::CreateSession {
                    command: echo_cmd(),
                    cwd: String::new(),
                    cols: 80,
                    rows: 24,
                },
            );
        }
        let first: Vec<u64> =
            registry.list(config().host).iter().map(|s| s.addr.session.0).collect();
        let second: Vec<u64> =
            registry.list(config().host).iter().map(|s| s.addr.session.0).collect();
        assert_eq!(first, second);
        assert!(first.windows(2).all(|w| w[0] < w[1]), "not sorted: {first:?}");
    }

    #[test]
    fn every_session_is_addressed_with_this_host() {
        // The fleet property. A session named without its host is unreachable
        // from a client holding sessions from several machines.
        let (mut c, registry) = conn();
        send(&mut c, &hello());
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        for info in registry.list(config().host) {
            assert_eq!(info.addr.host, config().host);
        }
    }
}
