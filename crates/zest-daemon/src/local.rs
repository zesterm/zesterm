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

    /// Accept clients until the process ends.
    pub fn listen(
        path: &str,
        config: DaemonConfig,
        registry: Arc<Registry>,
    ) -> Result<(), DaemonError> {
        // A socket left behind by a crashed daemon blocks binding, and the
        // owning process is gone, so removing it is safe *and* necessary --
        // otherwise a crash means the daemon can never start again.
        let _ = std::fs::remove_file(path);

        let listener =
            UnixListener::bind(path).map_err(|e| DaemonError::Transport(e.to_string()))?;

        // 0600 before anyone can connect. Without it the socket inherits umask,
        // which on a permissive one leaves a shell open to every local user.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| DaemonError::Transport(e.to_string()))?;

        tracing::info!(path, "listening");
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            let registry = Arc::clone(&registry);
            // A thread per client. A handful of devices per machine does not
            // justify an async runtime, and the daemon's job is to sit still.
            std::thread::spawn(move || {
                // Two handles on one socket: reading and writing must proceed
                // independently, or a blocking read holds off every push.
                let Ok(write_half) = stream.try_clone() else { return };
                if let Err(e) = crate::server::serve(stream, write_half, config, registry) {
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
            std::thread::spawn(move || {
                let Ok(write_half) = stream.try_clone() else { return };
                if let Err(e) = crate::server::serve(stream, write_half, config, registry) {
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

#[cfg(test)]
mod tests {
    use super::*;
    // Imported here rather than at module level: the unix backend needs neither,
    // so a module-level import is an unused-import error on macOS and Linux and
    // invisible on Windows, where the pipe implementation happens to use both.
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    use zest_proto::{frame, ClientId, ClientMessage, HostId, HostMessage, PROTOCOL_VERSION};

    fn config(path: &str) -> DaemonConfig {
        DaemonConfig {
            host: HostId::from_bytes([9; 32]),
            label: "loopback-test".into(),
            local_socket: path.to_string(),
            listen_lan: false,
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
                let _ = listen(&path, cfg, registry);
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

        let hello = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([2; 32]),
            label: "test-client".into(),
            nonce: zest_proto::Nonce32::from_bytes([5; 32]),
        };
        stream.write_all(&frame::encode(&hello).expect("encode")).expect("write");
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
        let mut reader = zest_proto::FrameReader::new();
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
