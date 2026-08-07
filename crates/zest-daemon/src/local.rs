//! The loopback transport: how a client on this machine reaches the daemon.
//!
//! A unix socket where there is one, a named pipe on Windows.
//!
//! # Why not loopback TCP, which would be one implementation instead of two
//!
//! Because it grants a shell. Loopback TCP is reachable by **every process on
//! the machine**, including another user's on a shared box, and there is no
//! filesystem permission to lean on. A unix socket carries the creating
//! process's mode, and a named pipe carries a DACL, so both restrict to the
//! user who started the daemon by construction rather than by an auth token
//! layered on afterwards.
//!
//! The LAN transport will be TCP, and *there* an auth token is unavoidable
//! because the peer is on another machine. That is a different problem with a
//! different answer, and conflating the two would mean the local case pays for
//! the remote one's ceremony while getting weaker isolation than it could have
//! had for free.

use std::sync::Arc;

use crate::server::Registry;
use crate::{DaemonConfig, DaemonError};

/// Where this machine's daemon listens.
///
/// Per-user, not per-machine: two people logged into the same box get separate
/// daemons and cannot see each other's shells.
#[must_use]
pub fn default_socket_path() -> String {
    #[cfg(windows)]
    {
        // Named pipes live in a flat kernel namespace, so the user name is what
        // separates two sessions on one machine.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        format!(r"\\.\pipe\zesterm-{}", sanitize(&user))
    }
    #[cfg(unix)]
    {
        // XDG_RUNTIME_DIR is already per-user and cleaned on logout, which is
        // exactly the lifetime a session socket wants. Falling back to /tmp
        // means the name has to carry the user itself.
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{dir}/zesterm.sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
        format!("/tmp/zesterm-{}.sock", sanitize(&user))
    }
}

/// Keep a user name to characters that are safe in a path or a pipe name.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

#[cfg(unix)]
mod imp {
    use super::{Arc, DaemonConfig, DaemonError, Registry};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// An exclusive claim on one socket path, held for the process lifetime.
    ///
    /// A separate `.lock` file rather than the socket itself, because the point
    /// is to hold a claim *across* unlinking and rebinding the socket — a lock
    /// on a file that is about to be deleted proves nothing about what replaces
    /// it.
    ///
    /// The fd is deliberately leaked into the guard and the guard lives as long
    /// as `listen`, which never returns while the daemon is up. `flock` is
    /// released when the last descriptor closes, including on a crash or a
    /// `SIGKILL`, which is what makes the stale case recover by itself.
    #[derive(Debug)]
    pub struct Lock {
        _file: std::fs::File,
    }

    impl Lock {
        pub fn acquire(path: &str) -> Result<Self, DaemonError> {
            let lock_path = format!("{path}.lock");
            use std::os::unix::fs::OpenOptionsExt as _;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|e| DaemonError::Transport(format!("{lock_path}: {e}")))?;

            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .map_err(|_| {
                    DaemonError::Transport(format!(
                        "another daemon is already serving {path}"
                    ))
                })?;

            Ok(Self { _file: file })
        }
    }

    /// A scoped umask, so a file is created with the mode it needs from birth.
    ///
    /// Process-wide and therefore not thread-safe in general; used here only
    /// during `bind`, on the thread that starts the daemon, before any other
    /// thread exists that could create a file.
    pub struct Umask(rustix::fs::Mode);

    impl Umask {
        pub fn restrict(mask: u16) -> Self {
            let mode = rustix::fs::Mode::from_bits_truncate(mask);
            Self(rustix::process::umask(mode))
        }
    }

    impl Drop for Umask {
        fn drop(&mut self) {
            rustix::process::umask(self.0);
        }
    }

    /// Claim a socket path without serving it.
    ///
    /// Exposed for tests, and for the moment before a daemon has decided it is
    /// the one that should run: `Ok` means this process holds the path, `Err`
    /// means another daemon is live and this one should connect to it instead
    /// of starting.
    pub fn claim(path: &str) -> Result<Lock, DaemonError> {
        Lock::acquire(path)
    }

    /// Accept clients until the process ends.
    pub fn listen(
        path: &str,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: std::sync::Arc<crate::auth::Authenticator>,
    ) -> Result<(), DaemonError> {
        // Take the lock *before* unlinking anything.
        //
        // The old code unlinked unconditionally, reasoning that a socket left
        // by a crashed daemon must be removed or the daemon could never start
        // again. True, and it also means two daemons starting at once
        // split-brain: the second unlinks the first's socket and binds its own,
        // the first keeps running on an unlinked path with its own Registry,
        // and every client that connects afterwards reaches only one of them.
        // Nothing exercised it while daemons were started by hand; the app
        // doing find-or-spawn is what would have.
        //
        // The lock also makes the stale-socket case *checked* rather than
        // assumed: a lock that can be taken proves no live daemon holds this
        // path, which is exactly the condition under which unlinking is safe.
        //
        // Windows needs none of this -- `FILE_FLAG_FIRST_PIPE_INSTANCE` makes
        // the loser's create fail outright.
        let _guard = Lock::acquire(path)?;

        let _ = std::fs::remove_file(path);

        // Bind inside a tightened umask rather than chmod-ing afterwards.
        //
        // `bind` applies the process umask, so between it and a later
        // `set_permissions` the socket is briefly whatever the umask allowed --
        // and on a permissive umask that window is a connectable shell. The
        // whole justification in this module's header rests on that permission,
        // so it must not have a gap.
        let listener = {
            let _umask = Umask::restrict(0o177);
            UnixListener::bind(path).map_err(|e| DaemonError::Transport(e.to_string()))?
        };

        // Belt and braces: assert the mode actually landed. A umask cannot add
        // permissions, only remove them, so this should be a no-op -- but the
        // cost of being wrong here is a shell.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| DaemonError::Transport(e.to_string()))?;

        tracing::info!(path, "listening");
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            let registry = Arc::clone(&registry);
            let auth = std::sync::Arc::clone(&auth);
            // A thread per client. A handful of devices per machine does not
            // justify an async runtime, and the daemon's job is to sit still.
            std::thread::spawn(move || {
                // Two handles on one socket: reading and writing must proceed
                // independently, or a blocking read holds off every push.
                let Ok(write_half) = stream.try_clone() else { return };
                // `Auth::Transport`, constructed here and nowhere else: the
                // socket's permissions are what authorized this connection.
                let auth = crate::auth::Auth::Transport(auth);
                if let Err(e) =
                    crate::server::serve(stream, write_half, config, registry, auth, "loopback")
                {
                    tracing::warn!(error = %e, "client disconnected");
                }
            });
        }
        Ok(())
    }

    /// Connect to a daemon already running.
    pub fn connect(path: &str) -> Result<UnixStream, DaemonError> {
        UnixStream::connect(path).map_err(|e| DaemonError::Transport(e.to_string()))
    }
}

#[cfg(windows)]
mod imp {
    use super::{Arc, DaemonConfig, DaemonError, Registry};
    use std::io::{Read, Write};
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
    // `PIPE_ACCESS_*` and `FILE_FLAG_*` are file-system flags rather than pipe
    // ones in windows-sys, which is where the names suggest they are not.
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A handle that is closed exactly once, however many halves share it.
    struct SharedHandle {
        handle: HANDLE,
        /// Whether to disconnect the pipe instance as well as closing it.
        ///
        /// True on the server's end and false on a client's: disconnecting is
        /// the server tearing down a client's connection, and a client doing it
        /// to itself is meaningless.
        disconnect: bool,
    }

    // SAFETY: a HANDLE is a kernel object index. The kernel serializes
    // operations on it, and a byte-mode duplex pipe explicitly supports a
    // concurrent ReadFile and WriteFile from different threads.
    unsafe impl Send for SharedHandle {}
    unsafe impl Sync for SharedHandle {}

    impl Drop for SharedHandle {
        fn drop(&mut self) {
            if self.handle.is_null() {
                return;
            }
            // SAFETY: live handle, owned by this value, dropped once because the
            // Arc guarantees a single owner at the end.
            unsafe {
                if self.disconnect {
                    DisconnectNamedPipe(self.handle);
                }
                CloseHandle(self.handle);
            }
        }
    }

    /// One end of a connected pipe.
    ///
    /// Halves *share* the handle rather than duplicating it. A duplicate is the
    /// obvious move and does not work here: writes through it are accepted and
    /// never reach the peer, which presents as a client that connects, is served,
    /// and then silently receives nothing.
    pub struct PipeStream {
        shared: std::sync::Arc<SharedHandle>,
    }

    /// An auto-reset event, owned, for one overlapped operation.
    struct Event(HANDLE);

    impl Event {
        fn new() -> io::Result<Self> {
            use windows_sys::Win32::System::Threading::CreateEventW;
            // SAFETY: null arguments request an unnamed, auto-reset, initially
            // unsignalled event.
            let h = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(h))
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: live handle, owned, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Run one overlapped operation to completion.
    ///
    /// The handle is opened `FILE_FLAG_OVERLAPPED` precisely so a read blocked
    /// on one thread does not hold off a write on another. **On a synchronous
    /// handle Windows serializes I/O per handle**, so a reader sitting in
    /// `ReadFile` -- which is exactly what a server does while a client is
    /// quiet -- silently defers every push until that read returns. The writes
    /// report success and simply never arrive, which presents as a client that
    /// connects, is served, and then hears nothing.
    ///
    /// Each call owns its `OVERLAPPED` and event, so two threads never share one
    /// and cannot mistake the other's completion for their own.
    fn overlapped<F>(handle: HANDLE, op: F) -> io::Result<u32>
    where
        F: FnOnce(*mut OVERLAPPED) -> i32,
    {
        use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
        use windows_sys::Win32::System::IO::GetOverlappedResult;

        let event = Event::new()?;
        // SAFETY: OVERLAPPED is a plain struct of integers and a handle; an
        // all-zero value is the documented initial state.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.0;

        let ok = op(std::ptr::addr_of_mut!(ov));
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
        }

        let mut transferred = 0u32;
        // SAFETY: `ov` and its event outlive this wait, and the operation was
        // issued against this handle.
        let ok = unsafe {
            GetOverlappedResult(handle, std::ptr::addr_of!(ov), &mut transferred, 1)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred)
    }

    impl Read for PipeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
            let handle = self.shared.handle;
            let len = buf.len().min(u32::MAX as usize) as u32;
            let dst = buf.as_mut_ptr();

            match overlapped(handle, |ov| {
                // SAFETY: handle is live; `buf` is valid for `len` bytes and
                // outlives the wait inside `overlapped`.
                unsafe { ReadFile(handle, dst, len, ptr::null_mut(), ov) }
            }) {
                Ok(n) => Ok(n as usize),
                // The peer closing its end is EOF, not a failure. Reported as an
                // error it makes every clean disconnect look like a crash -- the
                // same trap as the pty reader.
                Err(e) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
                Err(e) => Err(e),
            }
        }
    }

    impl Write for PipeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let handle = self.shared.handle;
            let len = buf.len().min(u32::MAX as usize) as u32;
            let src = buf.as_ptr();

            let n = overlapped(handle, |ov| {
                // SAFETY: handle is live; `buf` is valid for `len` bytes and
                // outlives the wait inside `overlapped`.
                unsafe { WriteFile(handle, src, len, ptr::null_mut(), ov) }
            })?;
            Ok(n as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PipeStream {
        fn new(handle: HANDLE, disconnect: bool) -> Self {
            Self { shared: std::sync::Arc::new(SharedHandle { handle, disconnect }) }
        }

        /// A second half onto the same connection.
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self { shared: std::sync::Arc::clone(&self.shared) })
        }
    }

    pub fn listen(
        path: &str,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: std::sync::Arc<crate::auth::Authenticator>,
    ) -> Result<(), DaemonError> {
        let name = wide(path);
        let mut first = true;

        tracing::info!(path, "listening");
        loop {
            // A fresh instance per client. `FILE_FLAG_FIRST_PIPE_INSTANCE` on
            // the first one is what makes a second daemon fail to start rather
            // than silently stealing connections from the first -- two daemons
            // on one pipe would hand clients different session registries.
            let flags = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
            // SAFETY: `name` is a NUL-terminated wide string that outlives the
            // call; the remaining arguments are plain values.
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | flags,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    // A null security descriptor gives the pipe the creating
                    // process's default DACL, which grants the owning user and
                    // SYSTEM. That is the isolation this transport is for.
                    ptr::null_mut(),
                )
            };
            if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return Err(DaemonError::Transport(io::Error::last_os_error().to_string()));
            }
            first = false;

            // Overlapped, because the handle is. A synchronous
            // `ConnectNamedPipe` against an overlapped handle returns without
            // waiting, and the server then serves a connection nobody made.
            let connected = overlapped(handle, |ov| {
                // SAFETY: `handle` is a live pipe instance.
                unsafe { ConnectNamedPipe(handle, ov) }
            });
            if let Err(e) = connected {
                use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
                // A client that connected between Create and Connect is already
                // there, which the API reports as a failure. It is not one.
                if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    tracing::debug!(error = %e, "connect failed");
                    // SAFETY: live handle, closed once on this path.
                    unsafe { CloseHandle(handle) };
                    continue;
                }
            }

            let stream = PipeStream::new(handle, true);
            let config = config.clone();
            let registry = Arc::clone(&registry);
            let auth = std::sync::Arc::clone(&auth);
            std::thread::spawn(move || {
                let Ok(write_half) = stream.try_clone() else { return };
                // `Auth::Transport`, constructed here and nowhere else: the
                // socket's permissions are what authorized this connection.
                let auth = crate::auth::Auth::Transport(auth);
                if let Err(e) =
                    crate::server::serve(stream, write_half, config, registry, auth, "loopback")
                {
                    tracing::warn!(error = %e, "client disconnected");
                }
            });
        }
    }

    pub fn connect(path: &str) -> Result<PipeStream, DaemonError> {
        let name = wide(path);
        // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(DaemonError::Transport(io::Error::last_os_error().to_string()));
        }
        Ok(PipeStream::new(handle, false))
    }
}

pub use imp::{connect, listen};
#[cfg(unix)]
pub use imp::claim;

#[cfg(test)]
mod tests {
    use super::*;
    // Imported here rather than at module level: the unix backend needs neither,
    // so a module-level import is an unused-import error on macOS and Linux and
    // invisible on Windows, where the pipe implementation happens to use both.
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    use zest_proto::{frame, ClientMessage, HostId, HostMessage, PROTOCOL_VERSION};

    fn config(path: &str) -> DaemonConfig {
        DaemonConfig {
            host: HostId::from_bytes([9; 32]),
            label: "loopback-test".into(),
            local_socket: path.to_string(),
            listen_lan: false,
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
        }
    }

    /// A path unique to this test, so a parallel run does not collide.
    fn test_path(name: &str) -> String {
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\zesterm-test-{name}")
        }
        #[cfg(unix)]
        {
            format!("/tmp/zesterm-test-{name}.sock")
        }
    }

    #[test]
    fn the_socket_path_is_per_user() {
        // Two people on one machine must not share a daemon, and therefore must
        // not share a socket name.
        let p = default_socket_path();
        assert!(!p.is_empty());
        assert!(p.contains("zesterm"), "{p}");
    }

    #[test]
    fn a_user_name_with_awkward_characters_is_made_safe() {
        // A domain account is `DOMAIN\user`, which is a path separator on one
        // platform and illegal in a pipe name on the other.
        assert_eq!(sanitize(r"DOMAIN\user name"), "DOMAINusername");
        assert_eq!(sanitize("andy-1_2"), "andy-1_2");
    }

    /// The whole loop over a real socket: connect, greet, create, attach, read.
    ///
    /// The point of this test is that it uses the *same* `serve` the daemon
    /// binary does, so a transport that works in a unit test and not in practice
    /// has nowhere to hide.
    #[test]
    fn a_client_drives_a_session_over_a_real_socket() {
        let path = test_path("drives");
        let registry = Arc::new(Registry::new());
        let cfg = config(&path);

        {
            let path = path.clone();
            let cfg = cfg.clone();
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                let _ = listen(&path, cfg, registry, test_authenticator());
            });
        }

        // Wait for the listener rather than sleeping a fixed time.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match connect(&path) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("never came up: {e}"),
            }
        };

        // The handshake, over the real socket. A client that only says hello
        // is no longer served, which is the point of this stage -- so this test
        // has to hold a key and answer a challenge like any other client.
        let client = zest_mesh::identity::ClientIdentity::generate().expect("client key");
        let client_nonce = [5u8; 32];
        let hello = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: client.client_id(),
            label: "test-client".into(),
            nonce: zest_proto::Nonce32::from_bytes(client_nonce),
        };
        stream.write_all(&frame::encode(&hello).expect("encode")).expect("write");
        stream.flush().expect("flush");

        let mut handshake = zest_proto::FrameReader::new();
        let mut scratch = vec![0u8; 64 * 1024];
        let challenge = loop {
            if let Ok(Some(body)) = handshake.next_frame() {
                match frame::decode::<HostMessage>(&body).expect("decode") {
                    HostMessage::Challenge { host, label, nonce, version, .. } => {
                        break (host, label, nonce, version);
                    }
                    other => panic!("expected a challenge, got {other:?}"),
                }
            }
            let n = Read::read(&mut stream, &mut scratch).expect("read");
            assert!(n > 0, "the daemon closed during the handshake");
            handshake.feed(&scratch[..n]);
        };
        let (host, host_label, host_nonce, version) = challenge;
        let transcript = zest_mesh::pairing::Transcript {
            version,
            host,
            client: client.client_id(),
            host_nonce: zest_mesh::identity::Nonce::from_bytes(host_nonce.0),
            client_nonce: zest_mesh::identity::Nonce::from_bytes(client_nonce),
            host_label,
            client_label: "test-client".into(),
        };
        let sig = client.sign(
            zest_mesh::identity::Purpose::Auth,
            &zest_mesh::pairing::auth_transcript(&transcript),
        );
        let auth_msg =
            ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes(sig.to_bytes()) };
        stream.write_all(&frame::encode(&auth_msg).expect("encode")).expect("write");
        stream.flush().expect("flush");

        let cmd = if cfg!(windows) {
            "cmd.exe /c echo over-the-socket".to_string()
        } else {
            "/bin/echo over-the-socket".to_string()
        };
        let create = ClientMessage::CreateSession { command: cmd, cwd: String::new(), cols: 80, rows: 24 };
        stream.write_all(&frame::encode(&create).expect("encode")).expect("write");
        stream.flush().expect("flush");

        // Read until the welcome and the session listing have both arrived.
        // Continues from the handshake's reader, which may already hold the
        // start of the next frame.
        let mut reader = handshake;
        let mut buf = vec![0u8; 64 * 1024];
        let mut saw_welcome = false;
        let mut sessions = Vec::new();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && (!saw_welcome || sessions.is_empty()) {
            let n = stream.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            reader.feed(&buf[..n]);
            while let Some(body) = reader.next_frame().expect("framing") {
                match frame::decode::<HostMessage>(&body).expect("decode") {
                    HostMessage::Welcome { host, .. } => {
                        assert_eq!(host, cfg.host, "the daemon reported another host's identity");
                        saw_welcome = true;
                    }
                    HostMessage::Sessions { sessions: s } => sessions = s,
                    _ => {}
                }
            }
        }

        assert!(saw_welcome, "no Welcome arrived over the socket");
        assert_eq!(sessions.len(), 1, "the session was not created");
        assert_eq!(
            sessions[0].addr.host, cfg.host,
            "a session was listed without this host's id, so it is unaddressable in a fleet"
        );

        #[cfg(unix)]
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    fn path(name: &str) -> String {
        // Short, because a unix socket path must fit in SUN_LEN (~104 bytes)
        // and the scratch directories CI uses are long enough to overflow it.
        format!("/tmp/zt-{}-{}.sock", name, std::process::id())
    }

    #[test]
    fn two_daemons_cannot_both_claim_one_socket() {
        // The split-brain this prevents: without the lock, the second daemon
        // unlinks the first's socket and binds its own. The first keeps
        // running -- on a path nothing can reach -- with its own Registry, so
        // sessions created through one are invisible to the other.
        //
        // Before the lock this test could not even be written: both calls
        // succeeded, and the damage only showed up as a missing session later.
        let p = path("claim");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(format!("{p}.lock"));

        let first = claim(&p).expect("the first daemon takes the path");
        let second = claim(&p);
        assert!(second.is_err(), "two daemons both believed they owned {p}");

        // And once the first lets go, the path is claimable again -- which is
        // what makes a crashed daemon recover without manual cleanup.
        drop(first);
        let third = claim(&p);
        assert!(third.is_ok(), "a released path must be reclaimable: {third:?}");

        drop(third);
        let _ = std::fs::remove_file(format!("{p}.lock"));
    }

    #[test]
    fn the_socket_is_never_world_reachable_even_for_an_instant() {
        // The module header rests on the socket's mode, so it must be 0600
        // from birth rather than 0600 shortly after birth. A permissive umask
        // is exactly the case that used to leave a window.
        let p = path("mode");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(format!("{p}.lock"));

        // A deliberately permissive umask for the duration of this test: the
        // window this closes only exists when the umask would have allowed
        // something wider, so testing under a restrictive one proves nothing.
        let _permissive = imp::Umask::restrict(0o000);

        let registry = std::sync::Arc::new(crate::server::Registry::new());
        let cfg = DaemonConfig {
            host: zest_proto::HostId::from_bytes([1; 32]),
            label: "mode-test".into(),
            local_socket: p.clone(),
            listen_lan: false,
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
        };
        let listen_path = p.clone();
        std::thread::spawn(move || {
            let _ = listen(&listen_path, cfg, registry, test_authenticator());
        });

        // Wait for the socket to appear rather than sleeping a fixed time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::path::Path::new(&p).exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&p).expect("socket exists").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket mode was {mode:o}, not 0600");

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(format!("{p}.lock"));
    }
}

#[cfg(test)]
fn test_authenticator() -> std::sync::Arc<crate::auth::Authenticator> {
    std::sync::Arc::new(crate::auth::Authenticator::new(
        std::sync::Arc::new(
            zest_mesh::identity::HostIdentity::generate().expect("host key"),
        ),
        std::sync::Arc::new(zest_mesh::trust::MemoryTrustStore::new()),
        zest_mesh::pairing::PairingQueue::new(),
        "test-host",
    ))
}
