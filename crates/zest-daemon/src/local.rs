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
    socket_path_for("zesterm")
}

/// A per-user local endpoint by service name, on the daemon's own rules.
///
/// `zesterm` is the daemon; `zesterm-app` is the running app's rendezvous for
/// a second launch (#497). One rule for every name, so a second endpoint
/// cannot drift into a different idea of "per user" than the first.
#[must_use]
pub fn socket_path_for(name: &str) -> String {
    #[cfg(windows)]
    {
        // Named pipes live in a flat kernel namespace, so the user name is what
        // separates two sessions on one machine.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        format!(r"\\.\pipe\{name}-{}", sanitize(&user))
    }
    #[cfg(unix)]
    {
        // XDG_RUNTIME_DIR is already per-user and cleaned on logout, which is
        // exactly the lifetime a session socket wants. Falling back to /tmp
        // means the name has to carry the user itself.
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{dir}/{name}.sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
        format!("/tmp/{name}-{}.sock", sanitize(&user))
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

    /// Bind the socket so it is never reachable by another user, even briefly.
    ///
    /// The module header rests on the socket's mode: `Auth::Transport` means
    /// the permission *is* the authorization, so the socket must be 0600 from
    /// birth rather than 0600 shortly after birth.
    ///
    /// The obvious tool — a scoped umask around `bind` — is banned here, and
    /// was #403: umask is process-global, so two threads binding at once (the
    /// zest-app test binary runs ~22 daemons on libtest's pool) race their
    /// save/restore and can leave the whole process restricted, after which
    /// every directory any *other* thread creates is born without owner-x and
    /// the first write inside it fails EACCES — in a crate far from this one.
    ///
    /// `mkdir(2)`'s explicit mode needs no global state: a umask can only
    /// remove bits from the requested 0700, never widen it, so the staging
    /// directory is private from birth. The socket binds inside it, is
    /// tightened to 0600 (chmod does not consult the umask), and is renamed
    /// into place — binding is to the inode, so the listener never notices,
    /// and the socket appears at the public path already 0600. The `.d/s`
    /// suffix spends 4 bytes of SUN_LEN (~104); test paths stay short partly
    /// for this reason.
    pub fn bind_private(path: &str) -> Result<UnixListener, DaemonError> {
        let stage = format!("{path}.d");
        // A stale stage is usually a directory from a crashed daemon, but the
        // name could in principle be squatted by anything; clear both shapes,
        // or the mkdir below wedges startup on EEXIST/ENOTDIR.
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_file(&stage);
        rustix::fs::mkdir(stage.as_str(), rustix::fs::Mode::from_bits_truncate(0o700))
            .map_err(|e| DaemonError::Transport(format!("{stage}: {e}")))?;
        // A pathological inherited umask can only have *narrowed* the 0700;
        // put the owner bits back so the bind below can proceed. No window
        // opens: the directory was never group- or other-accessible.
        std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| DaemonError::Transport(format!("{stage}: {e}")))?;

        // Every error names the staged path: a socket path within 4 bytes of
        // SUN_LEN fails *here* rather than at the final path, and the message
        // has to say which name was too long.
        let staged = format!("{stage}/s");
        let listener = UnixListener::bind(&staged)
            .map_err(|e| DaemonError::Transport(format!("{staged}: {e}")))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| DaemonError::Transport(format!("{staged}: {e}")))?;
        std::fs::rename(&staged, path)
            .map_err(|e| DaemonError::Transport(format!("{staged} -> {path}: {e}")))?;
        let _ = std::fs::remove_dir(&stage);
        Ok(listener)
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

    /// One end of a connected local stream.
    pub type LocalStream = UnixStream;

    /// An exclusive claim on a path *and* the socket bound at it, for as long
    /// as the value lives.
    ///
    /// What `listen` did inline, as a value, so that a second service on this
    /// machine — the app's own rendezvous for a second launch (#497) — binds
    /// by the same claim-unlink-bind sequence rather than by a second copy of
    /// it, which is how one of the two would come to skip the claim.
    pub struct LocalListener {
        _lock: Lock,
        listener: UnixListener,
        path: String,
    }

    impl LocalListener {
        pub fn bind_exclusive(path: &str) -> Result<Self, DaemonError> {
            // Take the lock *before* unlinking anything.
            //
            // The old code unlinked unconditionally, reasoning that a socket
            // left by a crashed daemon must be removed or the daemon could
            // never start again. True, and it also means two daemons starting
            // at once split-brain: the second unlinks the first's socket and
            // binds its own, the first keeps running on an unlinked path with
            // its own Registry, and every client that connects afterwards
            // reaches only one of them. Nothing exercised it while daemons
            // were started by hand; the app doing find-or-spawn is what would
            // have.
            //
            // The lock also makes the stale-socket case *checked* rather than
            // assumed: a lock that can be taken proves no live daemon holds
            // this path, which is exactly the condition under which unlinking
            // is safe.
            //
            // Windows needs none of this -- `FILE_FLAG_FIRST_PIPE_INSTANCE`
            // makes the loser's create fail outright.
            let lock = Lock::acquire(path)?;
            let _ = std::fs::remove_file(path);
            let listener = bind_private(path)?;
            Ok(Self { _lock: lock, listener, path: path.to_string() })
        }

        /// The next client. `&mut` for symmetry with the Windows end, which
        /// keeps the next pipe instance in the value.
        pub fn accept(&mut self) -> std::io::Result<LocalStream> {
            self.listener.accept().map(|(stream, _)| stream)
        }
    }

    impl Drop for LocalListener {
        /// Unlinked while the lock is still held (a `Drop` body runs before
        /// the fields drop), so no claimant can bind between the two.
        /// The daemon's `listen` never returns and never reaches this; the
        /// app's server does, on a clean exit, and a stale socket it left
        /// behind would cost every later launch a failed connect.
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}.lock", self.path));
        }
    }

    /// Accept clients until the process ends.
    pub fn listen(
        path: &str,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: std::sync::Arc<crate::auth::Authenticator>,
    ) -> Result<(), DaemonError> {
        let mut listener = LocalListener::bind_exclusive(path)?;

        tracing::info!(path, "listening");
        loop {
            let Ok(stream) = listener.accept() else { continue };
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
    }

    /// Connect to a daemon already running.
    pub fn connect(path: &str) -> Result<UnixStream, DaemonError> {
        UnixStream::connect(path).map_err(|e| DaemonError::Transport(e.to_string()))
    }
}

#[cfg(windows)]
mod imp {
    use super::{Arc, DaemonConfig, DaemonError, Registry};
    use std::ffi::OsStr;
    use std::io;
    // `Read`/`Write` are implemented for `PipeStream` below, so this import is
    // load-bearing here and nowhere else in the file -- the unix backend gets
    // them from `UnixStream`. A tidy-up that deletes it because macOS says
    // "unused" breaks only Windows, and only in CI.
    use std::io::{Read, Write};
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

        /// Abort whatever another thread has parked in on this handle.
        ///
        /// There is no read timeout on an overlapped pipe — `GetOverlappedResult`
        /// waits forever — so a caller that gave up on a peer has to unpark its
        /// reader from outside: the pending read completes with
        /// `ERROR_OPERATION_ABORTED`, the reader returns, and the thread ends
        /// instead of holding a handle for the life of the process.
        pub fn cancel_io(&self) {
            use windows_sys::Win32::System::IO::CancelIoEx;
            // SAFETY: a live handle; a null OVERLAPPED cancels every pending
            // operation on it from any thread.
            unsafe { CancelIoEx(self.shared.handle, ptr::null()) };
        }
    }

    /// One end of a connected local stream.
    pub type LocalStream = PipeStream;

    /// An exclusive claim on a pipe name, and the instance waiting for the
    /// next client.
    ///
    /// The unix end's shape (claim, then accept), so a second service — the
    /// app's rendezvous for a second launch (#497) — shares this loop rather
    /// than copying it: the overlapped `ConnectNamedPipe` and the
    /// `ERROR_PIPE_CONNECTED` arm below are the paid-for trap, and a copy is
    /// how one of them loses it.
    pub struct LocalListener {
        name: Vec<u16>,
        /// The instance created ahead of the next `accept`, so a client
        /// arriving between two accepts finds an instance to connect to
        /// rather than `ERROR_FILE_NOT_FOUND`.
        next: HANDLE,
    }

    // SAFETY: a HANDLE is a kernel object index; the value moves to the
    // thread that accepts, and nothing else holds it.
    unsafe impl Send for LocalListener {}

    impl LocalListener {
        fn instance(name: &[u16], first: bool) -> Result<HANDLE, DaemonError> {
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
            Ok(handle)
        }

        /// Claim the name: `Err` means another process already serves it.
        /// Eager, so "already owned" is known at bind time rather than on
        /// the first accept.
        pub fn bind_exclusive(path: &str) -> Result<Self, DaemonError> {
            let name = wide(path);
            let next = Self::instance(&name, true)?;
            Ok(Self { name, next })
        }

        /// The next client.
        pub fn accept(&mut self) -> io::Result<LocalStream> {
            loop {
                let handle = if self.next.is_null() {
                    Self::instance(&self.name, false).map_err(|e| io::Error::other(e.to_string()))?
                } else {
                    std::mem::replace(&mut self.next, ptr::null_mut())
                };

                // Overlapped, because the handle is. A synchronous
                // `ConnectNamedPipe` against an overlapped handle returns
                // without waiting, and the server then serves a connection
                // nobody made.
                let connected = overlapped(handle, |ov| {
                    // SAFETY: `handle` is a live pipe instance.
                    unsafe { ConnectNamedPipe(handle, ov) }
                });
                if let Err(e) = connected {
                    use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
                    // A client that connected between Create and Connect is
                    // already there, which the API reports as a failure. It
                    // is not one.
                    if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                        tracing::debug!(error = %e, "connect failed");
                        // SAFETY: live handle, closed once on this path.
                        unsafe { CloseHandle(handle) };
                        continue;
                    }
                }
                // The next instance now, before this client is served: the
                // gap in which nothing listens is what a second launcher
                // would otherwise fall into.
                self.next = Self::instance(&self.name, false).unwrap_or(ptr::null_mut());
                return Ok(PipeStream::new(handle, true));
            }
        }
    }

    impl Drop for LocalListener {
        fn drop(&mut self) {
            if !self.next.is_null() {
                // SAFETY: live handle, owned, closed once.
                unsafe { CloseHandle(self.next) };
            }
        }
    }

    pub fn listen(
        path: &str,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: std::sync::Arc<crate::auth::Authenticator>,
    ) -> Result<(), DaemonError> {
        let mut listener = LocalListener::bind_exclusive(path)?;

        tracing::info!(path, "listening");
        loop {
            let stream = listener.accept().map_err(|e| DaemonError::Transport(e.to_string()))?;
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
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
        let name = wide(path);
        let handle = loop {
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
            if handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                break handle;
            }
            let err = io::Error::last_os_error();
            // Every instance is mid-accept. The server creates the next one
            // right after each connect, so this is a moment, not a state;
            // wait for it rather than reporting a server that is plainly up
            // as absent.
            // SAFETY: `name` outlives the call.
            if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                && unsafe { WaitNamedPipeW(name.as_ptr(), 2000) } != 0
            {
                continue;
            }
            return Err(DaemonError::Transport(err.to_string()));
        };
        Ok(PipeStream::new(handle, false))
    }
}

pub use imp::{connect, listen, LocalListener, LocalStream};
#[cfg(unix)]
pub use imp::claim;
// `connect` returns this, so leaving it unexported made its own return type
// unnameable: `zest-app` could hold the stream but not write a signature taking
// one. The unix side never noticed, because there `connect` returns a
// `std::os::unix::net::UnixStream` that every caller can already name.
#[cfg(windows)]
pub use imp::PipeStream;

// Shared by both test modules below, so it cannot live inside either, and
// clippy's `items_after_test_module` insists it come before them.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use zest_proto::HostId;

    fn config(path: &str) -> DaemonConfig {
        DaemonConfig {
            host: HostId::from_bytes([9; 32]),
            label: "loopback-test".into(),
            local_socket: path.to_string(),
            listen_lan: false,
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
            listen_ws: false,
            ws_bind: "127.0.0.1".into(),
            ws_port: 0,
            relay: None,
            shell_integration: true,
            min_delta_interval: Duration::ZERO,
            enroll: None,
            offer: None,
            settings: None,
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
    fn the_daemon_path_is_the_named_form_of_its_own_service() {
        // `default_socket_path` is the rule every client already relies on;
        // parameterising it by name must not have moved the daemon.
        assert_eq!(default_socket_path(), socket_path_for("zesterm"));
        let app = socket_path_for("zesterm-app");
        assert_ne!(app, default_socket_path(), "two services, two endpoints");
        assert!(app.contains("zesterm-app"), "{app}");
    }

    #[test]
    fn one_path_has_one_listener_at_a_time() {
        // The claim, on both platforms: `two_daemons_cannot_both_claim_one_socket`
        // covers the unix lock alone, and the Windows half
        // (`FILE_FLAG_FIRST_PIPE_INSTANCE`) had no test until a second
        // service came to depend on it.
        let p = test_path(&format!("excl-{}", std::process::id()));
        let first = LocalListener::bind_exclusive(&p).expect("the first listener takes the path");
        assert!(
            LocalListener::bind_exclusive(&p).is_err(),
            "two listeners both believed they owned {p}"
        );
        drop(first);
        let again = LocalListener::bind_exclusive(&p);
        assert!(again.is_ok(), "a released path must be reclaimable: {:?}", again.err());
        drop(again);
        #[cfg(unix)]
        {
            assert!(!std::path::Path::new(&p).exists(), "the socket outlived its listener");
            assert!(!std::path::Path::new(&format!("{p}.lock")).exists(), "the lock file outlived its listener");
        }
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
        let stream = loop {
            match connect(&path) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("never came up: {e}"),
            }
        };

        // The handshake, over the real socket, through the *same* client the
        // app uses. It was hand-rolled here until protocol 3, which was
        // survivable while a wrong peer failed at the signature; now the same
        // steps derive the key every later frame is encrypted under, and a
        // second implementation is a second key schedule to keep in step.
        let client = Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("client key"));
        let reader = stream.try_clone().expect("clone the socket");
        let mut conn = crate::client::DaemonClient::connect(
            Box::new(reader),
            Box::new(stream),
            &client,
            "test-client",
            None,
            false,
        )
        .expect("the daemon must serve a loopback client");

        assert_eq!(conn.host(), cfg.host, "the daemon reported another host's identity");

        let cmd = if cfg!(windows) {
            "cmd.exe /c echo over-the-socket".to_string()
        } else {
            "/bin/echo over-the-socket".to_string()
        };
        conn.create(&crate::client::Launch { command: &cmd, ..Default::default() }, 80, 24)
            .expect("create a session over the socket");
        let sessions = conn.list().expect("list sessions");

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

    /// Held by every test that reads or writes the process umask. It is one
    /// value per process — the very fact #403 was about — so the two tests
    /// below must not interleave, or the reader observes the setter's value
    /// and fails on a truth about the harness rather than the code.
    static UMASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        //
        // A save/restore guard is the very shape #403 bans — it is safe here
        // only because UMASK_LOCK serializes every umask toucher in this
        // binary, and Drop (not a trailing statement) is what keeps a failing
        // assertion from leaking the permissive value to later tests.
        let _serialized = UMASK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        struct Restore(rustix::fs::Mode);
        impl Drop for Restore {
            fn drop(&mut self) {
                rustix::process::umask(self.0);
            }
        }
        let _restore = Restore(rustix::process::umask(rustix::fs::Mode::empty()));

        let registry = std::sync::Arc::new(crate::server::Registry::new());
        let cfg = DaemonConfig {
            host: zest_proto::HostId::from_bytes([1; 32]),
            label: "mode-test".into(),
            local_socket: p.clone(),
            listen_lan: false,
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
            listen_ws: false,
            ws_bind: "127.0.0.1".into(),
            ws_port: 0,
            relay: None,
            shell_integration: true,
            min_delta_interval: std::time::Duration::ZERO,
            enroll: None,
            offer: None,
            settings: None,
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

    #[test]
    fn binding_never_mutates_the_umask_other_threads_read() {
        // The #403 shape: umask is process-global, so a bind that saves and
        // restores it races another bind doing the same -- B saves A's
        // restricted value as "previous" and restores *that*, leaving the
        // whole process restricted forever. The victims are bystanders: any
        // thread creating a directory afterwards gets one without owner-x,
        // and its first write inside fails EACCES. In `cargo test --workspace`
        // the bystander was zest-app's themes::tests, a crate away from the
        // culprit, and the symptom read as CI's temp root breaking.
        let _serialized = UMASK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = {
            let cur = rustix::process::umask(rustix::fs::Mode::empty());
            rustix::process::umask(cur);
            cur
        };

        // Concurrent binders, like the ~22 daemon harnesses the zest-app test
        // binary runs on libtest's thread pool.
        let binders: Vec<_> = (0..4)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..64 {
                        let p = path(&format!("um{t}x{i}"));
                        let _ = std::fs::remove_file(&p);
                        let listener =
                            imp::bind_private(&p).expect("a private bind on a fresh path");
                        drop(listener);
                        let _ = std::fs::remove_file(&p);
                    }
                })
            })
            .collect();

        // The bystander: a thread that just wants a scratch directory, as any
        // test (or the daemon's own audit/session code) might.
        let bystander = std::thread::spawn(|| {
            let dir = std::env::temp_dir()
                .join(format!("zesterm-umask-bystander-{}", std::process::id()));
            for _ in 0..256 {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).expect("bystander scratch dir");
                std::fs::write(dir.join("probe"), b"x").expect(
                    "a write into a directory this thread just created: EACCES here means \
                     a concurrent bind leaked a restrictive umask into the whole process",
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        });

        for b in binders {
            b.join().expect("binder thread");
        }
        let bystander = bystander.join();

        let after = {
            let cur = rustix::process::umask(rustix::fs::Mode::empty());
            rustix::process::umask(cur);
            cur
        };
        assert_eq!(
            after, before,
            "binding sockets changed the process umask, so every later file and \
             directory in this process is born with the wrong mode"
        );
        bystander.expect("bystander thread");
    }
}
