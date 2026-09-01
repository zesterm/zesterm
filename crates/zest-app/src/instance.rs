//! A second `zesterm` launch opens in the running one (#497).
//!
//! Warp does this; Windows Terminal's `wt` hands the request to its
//! "monarch" and `windowingBehavior` decides whether that becomes a window or
//! a tab. Without it a second launch is a second process: a second mDNS
//! browser, a second config watcher, a second Keychain prompt, and two
//! processes fighting over one remembered window set.
//!
//! The mechanism is a second per-user local endpoint beside the daemon's —
//! `zesterm-app`, a unix socket or a named pipe — served by the running
//! process on [`zest_daemon::LocalListener`], the daemon's own transport
//! lifted rather than copied. It is deliberately **not** the daemon's session
//! protocol: the daemon is a session server, and a request to *this app* to
//! open a window is nothing a session server should know how to answer.
//!
//! Three rules shape everything here:
//!
//! - **A launch never hangs.** The running instance may be wedged; the
//!   launcher waits [`FORWARD_BUDGET`] for an answer and then opens its own
//!   window. A window too many is recoverable; a launch that does nothing is
//!   not.
//! - **The instance answers `Ok` only after the window exists.** Its acceptor
//!   waits [`ANSWER_BUDGET`] — shorter than the launcher's — for the event
//!   loop, and says nothing at all when that elapses, so a hung UI cannot
//!   answer `Ok` late to a launcher that has already given up.
//! - **A different build is a different program.** [`BuildId`] carries the
//!   binary's own length and mtime, so `zesterm-dev`'s rebuilt binary never
//!   forwards to the stale one it is trying to replace, and no `build.rs` or
//!   git invocation is needed to tell the two apart.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use winit::event_loop::EventLoopProxy;
use zest_config::settings::LaunchTarget;
use zest_daemon::{LocalListener, LocalStream};
use zest_proto::frame;

use crate::app::StartScreen;
use crate::session::Wakeup;

/// Counts message layouts, independently of the daemon protocol's version.
pub const PROTOCOL: u16 = 1;

/// The endpoint's service name, beside the daemon's `zesterm`.
pub const SERVICE: &str = "zesterm-app";

/// How long a launch waits for the running instance before opening its own
/// window. Long enough for a warm event loop to open a window and answer;
/// short enough that a wedged instance does not read as a broken launcher.
pub const FORWARD_BUDGET: Duration = Duration::from_millis(500);

/// How long the acceptor waits for the event loop to act on a request.
/// **Shorter than [`FORWARD_BUDGET`]**, so an answer that misses this is
/// never written: the launcher has moved on, and an `Ok` it never reads is
/// harmless, but an `Ok` it reads *late* would leave it exiting with no
/// window anywhere.
const ANSWER_BUDGET: Duration = Duration::from_millis(400);

#[must_use]
pub fn socket_path() -> String {
    zest_daemon::socket_path_for(SERVICE)
}

/// Which binary is on each end. A mismatch means "not the same program".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    pub version: String,
    /// The executable's own size and mtime: one `stat`, and it distinguishes
    /// two builds of the same dirty tree, which a git sha would not.
    pub exe_len: u64,
    pub exe_mtime: u64,
}

impl BuildId {
    #[must_use]
    pub fn current() -> Self {
        let (exe_len, exe_mtime) = std::env::current_exe()
            .and_then(std::fs::metadata)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                (m.len(), mtime)
            })
            .unwrap_or((0, 0));
        Self { version: env!("CARGO_PKG_VERSION").to_string(), exe_len, exe_mtime }
    }
}

/// The first frame on a connection. Named fields (`to_vec_named`), so a
/// future field is not a break: both versions must decode *this* to learn
/// they differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u16,
    pub build: BuildId,
}

impl Hello {
    #[must_use]
    pub fn current() -> Self {
        Self { protocol: PROTOCOL, build: BuildId::current() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenTarget {
    Window,
    /// A tab in the instance's focused window.
    Tab,
}

/// What the launcher wants opened. Every field is one of its own flags, or
/// the directory it was run from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub target: OpenTarget,
    /// `-e`, already joined.
    pub command: Option<String>,
    /// `--profile`, meaning "launch this profile" — never the process-level
    /// cascade layer, which a running instance cannot take on.
    pub profile: Option<String>,
    /// Where the launcher was run: a second launch from a shell opens there.
    pub cwd: Option<String>,
    pub screen: Option<StartScreen>,
    /// `--attach <host:port>`: a window on another machine's daemon.
    pub attach: Option<String>,
    /// Wayland's `XDG_ACTIVATION_TOKEN`, so the instance may raise the new
    /// window under the launcher's own right to focus.
    pub activation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    /// The request cannot be honoured; the launcher prints this and exits 1.
    Refused(String),
    /// The instance is another build: the launcher opens its own window.
    OtherBuild,
}

/// The flags that decide a launch, as `main` parsed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchFlags {
    /// `--screenshot`, `--startup-probe`, `--attach-probe`: a measurement or a
    /// picture, never a user's window.
    pub probe: bool,
    /// A config layer of this process's own (`--theme`, `--size`, …),
    /// `--no-daemon`, `--simulated-latency`: a window opened on someone
    /// else's behalf must carry the default config, not this one's.
    pub own_config: bool,
    /// `--new-window` / `--new-tab` / `--new-instance`.
    pub target: Option<LaunchTarget>,
    pub command: Option<String>,
    pub profile: Option<String>,
    pub screen: Option<StartScreen>,
    pub attach: Option<String>,
}

/// What this launch does about a running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Ask the running instance to open this, if there is one.
    pub forward: Option<OpenRequest>,
    /// Serve later launches from this process, when nobody else does.
    pub serve: bool,
}

/// A screen that *is* a tab, so `--new-tab --screen settings` can mean one.
fn tab_shaped(screen: StartScreen) -> bool {
    matches!(screen, StartScreen::Settings | StartScreen::Profiles)
}

/// The rule, pure so the table below can pin it.
#[must_use]
pub fn classify(flags: &LaunchFlags, setting: LaunchTarget) -> Launch {
    if flags.probe || flags.own_config {
        return Launch { forward: None, serve: false };
    }
    let target = flags.target.unwrap_or(setting);
    if target == LaunchTarget::Instance {
        return Launch { forward: None, serve: true };
    }
    // A tab cannot be on another machine, and a full-pane screen is not a
    // tab: both fall back to a window rather than refusing a launch the
    // *setting* shaped.
    let target = match target {
        LaunchTarget::Tab
            if flags.attach.is_none() && flags.screen.is_none_or(tab_shaped) =>
        {
            OpenTarget::Tab
        }
        _ => OpenTarget::Window,
    };
    let request = OpenRequest {
        target,
        command: flags.command.clone(),
        profile: flags.profile.clone(),
        cwd: std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()),
        screen: flags.screen,
        attach: flags.attach.clone(),
        activation_token: activation_token(),
    };
    // `--profile` is also a cascade layer of *this* process, so a window
    // opened later on someone else's behalf would carry it; forward, but do
    // not serve.
    Launch { forward: Some(request), serve: flags.profile.is_none() }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn activation_token() -> Option<String> {
    std::env::var("XDG_ACTIVATION_TOKEN").ok().filter(|t| !t.is_empty())
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn activation_token() -> Option<String> {
    None
}

/// What forwarding came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The instance opened it; this process has nothing left to do.
    Opened,
    Refused(String),
    /// Nobody is serving the endpoint.
    NoInstance,
    /// Someone is, but it is not this binary.
    OtherBuild,
    /// Connected, but no answer inside the budget: the instance is wedged,
    /// or busy past what a launch should wait for.
    NoAnswer,
}

/// One whole frame from a stream, keeping bytes read past it in `reader`
/// for the next call — a launcher writes both its frames at once.
fn read_frame<R: Read>(
    stream: &mut R,
    reader: &mut frame::FrameReader,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(body) = reader.next_frame().map_err(std::io::Error::other)? {
            return Ok(Some(body));
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        reader.feed(&buf[..n]);
    }
}

fn write_frame<W: Write, T: Serialize>(stream: &mut W, msg: &T) -> std::io::Result<()> {
    let bytes = frame::encode(msg).map_err(std::io::Error::other)?;
    stream.write_all(&bytes)
}

/// Ask a running instance to open `request`, bounded by `budget`.
///
/// The I/O runs on a helper thread and the caller waits on a channel:
/// `PipeStream` has no read timeout (`GetOverlappedResult` waits forever), and
/// the READ_POLL lesson from the daemon says a peer that stays up and says
/// nothing is exactly the case a timeout exists for. On timeout the thread is
/// abandoned; this process either exits or becomes a window of its own, and
/// the handle closes with it.
#[must_use]
pub fn forward(path: &str, request: &OpenRequest, budget: Duration) -> Verdict {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let path = path.to_string();
    let request = request.clone();
    let spawned = std::thread::Builder::new().name("zesterm-forward".into()).spawn(move || {
        let _ = tx.send(forward_blocking(&path, &request, budget));
    });
    if spawned.is_err() {
        return Verdict::NoInstance;
    }
    rx.recv_timeout(budget).unwrap_or(Verdict::NoAnswer)
}

fn forward_blocking(path: &str, request: &OpenRequest, budget: Duration) -> Verdict {
    let Ok(mut stream) = zest_daemon::connect(path) else {
        return Verdict::NoInstance;
    };
    #[cfg(unix)]
    let _ = stream.set_read_timeout(Some(budget));
    #[cfg(windows)]
    let _ = budget;
    // Before the request, so the instance may bring its window to the front
    // under this process's right to the foreground — a background process
    // cannot take it for itself, by Windows' foreground-lock rules.
    allow_foreground();
    if write_frame(&mut stream, &Hello::current()).is_err()
        || write_frame(&mut stream, request).is_err()
    {
        return Verdict::NoInstance;
    }
    let mut reader = frame::FrameReader::new();
    match read_frame(&mut stream, &mut reader) {
        Ok(Some(body)) => match frame::decode::<Reply>(&body) {
            Ok(Reply::Ok) => Verdict::Opened,
            Ok(Reply::Refused(why)) => Verdict::Refused(why),
            Ok(Reply::OtherBuild) => Verdict::OtherBuild,
            Err(_) => Verdict::OtherBuild,
        },
        // EOF or a timeout: the instance chose not to answer, which is its
        // way of saying "open your own".
        Ok(None) | Err(_) => Verdict::NoAnswer,
    }
}

#[cfg(windows)]
fn allow_foreground() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    // SAFETY: a plain call with a constant argument.
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
}

#[cfg(not(windows))]
fn allow_foreground() {}

/// Serve one connection: check the greeting, take the request, write what
/// `answer` says — or nothing, when it says nothing.
fn serve_one(mut stream: LocalStream, answer: &dyn Fn(OpenRequest) -> Option<Reply>) {
    let mut reader = frame::FrameReader::new();
    let Ok(Some(hello)) = read_frame(&mut stream, &mut reader) else { return };
    let Ok(hello) = frame::decode::<Hello>(&hello) else {
        let _ = write_frame(&mut stream, &Reply::OtherBuild);
        return;
    };
    if hello != Hello::current() {
        let _ = write_frame(&mut stream, &Reply::OtherBuild);
        return;
    }
    let Ok(Some(request)) = read_frame(&mut stream, &mut reader) else { return };
    let Ok(request) = frame::decode::<OpenRequest>(&request) else {
        let _ = write_frame(&mut stream, &Reply::Refused("unreadable request".into()));
        return;
    };
    if let Some(reply) = answer(request) {
        let _ = write_frame(&mut stream, &reply);
    }
}

/// A request parked for the event loop, and the way to answer it.
pub struct PendingOpen {
    pub request: OpenRequest,
    pub reply: crossbeam_channel::Sender<Reply>,
}

pub type PendingOpens = Arc<parking_lot::Mutex<Vec<PendingOpen>>>;

/// The endpoint, claimed but not yet served.
///
/// Claimed in `main` before the window exists — a claim is one flock or one
/// `CreateNamedPipeW`, well under a millisecond — and served only once the
/// first window has painted, so nothing of this sits between creating the
/// window and showing it (ADR-007).
pub struct Claim {
    listener: LocalListener,
    path: String,
}

/// Claim the endpoint; `Err` means another process serves it.
pub fn claim() -> Result<Claim, String> {
    let path = socket_path();
    let listener = LocalListener::bind_exclusive(&path).map_err(|e| e.to_string())?;
    Ok(Claim { listener, path })
}

/// The running instance's end: accepts launches and parks them for the
/// event loop.
pub struct InstanceServer {
    path: String,
}

impl InstanceServer {
    pub fn start(claim: Claim, proxy: EventLoopProxy<Wakeup>, pending: PendingOpens) -> Self {
        let Claim { mut listener, path } = claim;
        let spawned = std::thread::Builder::new().name("zesterm-instance".into()).spawn(move || {
            loop {
                let stream = match listener.accept() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(error = %e, "instance accept failed");
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };
                let proxy = proxy.clone();
                let pending = Arc::clone(&pending);
                // A thread per launcher, the daemon's shape: one launcher
                // that stalls must not hold off the next.
                let _ = std::thread::Builder::new().name("zesterm-instance-conn".into()).spawn(
                    move || {
                        serve_one(stream, &|request| {
                            let (tx, rx) = crossbeam_channel::bounded(1);
                            pending.lock().push(PendingOpen { request, reply: tx });
                            if proxy.send_event(Wakeup::OpenRequested).is_err() {
                                return Some(Reply::Refused("zesterm is exiting".into()));
                            }
                            rx.recv_timeout(ANSWER_BUDGET).ok()
                        });
                    },
                );
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no thread for the instance endpoint; later launches open their own window");
        }
        Self { path }
    }
}

impl Drop for InstanceServer {
    /// The listener lives on the accept thread, parked in `accept`, and dies
    /// with the process; on unix the files it would have unlinked are
    /// unlinked here instead, so a clean exit leaves no stale socket for the
    /// next launch to fail a connect on. Windows pipes die with the process.
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}.lock", self.path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> LaunchFlags {
        LaunchFlags::default()
    }

    #[test]
    fn a_plain_launch_forwards_a_window_and_serves_when_alone() {
        let l = classify(&flags(), LaunchTarget::Window);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Window));
        assert!(l.serve, "the first launch is the one every later launch reaches");
        assert!(l.forward.unwrap().cwd.is_some(), "a launch from a shell opens where the shell was");
    }

    #[test]
    fn the_setting_picks_tab_and_a_flag_overrides_it() {
        let l = classify(&flags(), LaunchTarget::Tab);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Tab));
        let l = classify(&LaunchFlags { target: Some(LaunchTarget::Window), ..flags() }, LaunchTarget::Tab);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Window), "--new-window beats window.launch");
    }

    #[test]
    fn instance_means_a_separate_process_that_still_serves() {
        for l in [
            classify(&flags(), LaunchTarget::Instance),
            classify(&LaunchFlags { target: Some(LaunchTarget::Instance), ..flags() }, LaunchTarget::Window),
        ] {
            assert_eq!(l.forward, None, "--new-instance asks nobody");
            assert!(l.serve, "but it is still the instance a later plain launch reaches, when it is the only one");
        }
    }

    #[test]
    fn probes_and_own_config_neither_forward_nor_serve() {
        // A screenshot forwarded would photograph nothing; a `--theme`
        // process serving would open someone else's window in that theme.
        for f in [LaunchFlags { probe: true, ..flags() }, LaunchFlags { own_config: true, ..flags() }] {
            let l = classify(&f, LaunchTarget::Window);
            assert_eq!(l, Launch { forward: None, serve: false }, "{f:?}");
        }
    }

    #[test]
    fn a_profile_forwards_as_a_launch_but_does_not_serve() {
        let l = classify(&LaunchFlags { profile: Some("k8s".into()), ..flags() }, LaunchTarget::Window);
        assert_eq!(l.forward.as_ref().and_then(|r| r.profile.clone()).as_deref(), Some("k8s"));
        assert!(!l.serve, "--profile is also this process's cascade layer, which a window opened for someone else must not carry");
    }

    #[test]
    fn a_tab_cannot_be_on_another_machine_or_a_full_pane_screen() {
        let l = classify(&LaunchFlags { attach: Some("10.0.0.2:7717".into()), ..flags() }, LaunchTarget::Tab);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Window));
        let l = classify(&LaunchFlags { screen: Some(StartScreen::Fleet), ..flags() }, LaunchTarget::Tab);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Window), "the fleet screen is a pane, not a tab");
        let l = classify(&LaunchFlags { screen: Some(StartScreen::Settings), ..flags() }, LaunchTarget::Tab);
        assert_eq!(l.forward.as_ref().map(|r| r.target), Some(OpenTarget::Tab), "Settings is a tab, so a tab it is");
    }

    fn request() -> OpenRequest {
        OpenRequest {
            target: OpenTarget::Window,
            command: Some("htop".into()),
            profile: None,
            cwd: Some("/tmp".into()),
            screen: Some(StartScreen::Editor),
            attach: None,
            activation_token: None,
        }
    }

    #[test]
    fn the_messages_round_trip_and_tolerate_a_field_they_do_not_know() {
        let bytes = frame::encode_body(&request()).unwrap();
        assert_eq!(frame::decode::<OpenRequest>(&bytes).unwrap(), request());
        let bytes = frame::encode_body(&Hello::current()).unwrap();
        assert_eq!(frame::decode::<Hello>(&bytes).unwrap(), Hello::current());
        // A later build adds a field; this one must still read the frame,
        // because that is how it learns the other end is a later build.
        let extended = frame::encode_body(&serde_json::json!({
            "protocol": PROTOCOL, "build": {"version": "9", "exe_len": 1, "exe_mtime": 2}, "colour": "blue"
        }))
        .unwrap();
        let hello: Hello = frame::decode(&extended).expect("an unknown field is not a parse error");
        assert_ne!(hello, Hello::current());
    }

    fn test_path(name: &str) -> String {
        zest_daemon::socket_path_for(&format!("zt-inst-{name}-{}", std::process::id()))
    }

    /// The server half, in-process, answering `answer` once.
    fn serve_once(path: &str, answer: impl Fn(OpenRequest) -> Option<Reply> + Send + 'static) -> Claim {
        let mut listener = LocalListener::bind_exclusive(path).expect("claim the test endpoint");
        // The thread takes a second listener-shaped value's role: this test
        // wants the claim to stay alive on the test thread, so accept from a
        // clone of nothing — accept here, then hand the stream over.
        let (tx, rx) = crossbeam_channel::bounded::<LocalStream>(1);
        std::thread::spawn(move || {
            if let Ok(stream) = listener.accept() {
                let _ = tx.send(stream);
            }
            // Keep the listener alive until the test ends, else the unix
            // socket is unlinked under the launcher.
            std::thread::sleep(Duration::from_secs(5));
        });
        std::thread::spawn(move || {
            if let Ok(stream) = rx.recv_timeout(Duration::from_secs(5)) {
                serve_one(stream, &answer);
            }
        });
        Claim { listener: LocalListener::bind_exclusive(&format!("{path}-unused")).expect("a second name"), path: format!("{path}-unused") }
    }

    #[test]
    fn a_forwarded_request_is_answered_and_a_silent_server_is_not_waited_on() {
        let path = test_path("fwd");
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let saw = Arc::clone(&seen);
        let _claim = serve_once(&path, move |r| {
            *saw.lock() = Some(r);
            Some(Reply::Ok)
        });
        assert_eq!(forward(&path, &request(), Duration::from_secs(5)), Verdict::Opened);
        assert_eq!(seen.lock().as_ref(), Some(&request()), "the request arrived whole");

        // A peer that stays connected and says nothing — the READ_POLL
        // lesson: this is the case the budget exists for, so the socket
        // must stay open rather than be closed to make the test pass.
        let path = test_path("silent");
        let _claim = serve_once(&path, |_| {
            std::thread::sleep(Duration::from_secs(3));
            Some(Reply::Ok)
        });
        let started = std::time::Instant::now();
        let verdict = forward(&path, &request(), Duration::from_millis(300));
        assert_eq!(verdict, Verdict::NoAnswer);
        assert!(started.elapsed() < Duration::from_secs(2), "a launch must not wait on a wedged instance");

        assert_eq!(forward(&test_path("nobody"), &request(), Duration::from_secs(1)), Verdict::NoInstance);
    }

    #[test]
    fn another_build_is_told_so_and_opens_its_own_window() {
        let path = test_path("build");
        let _claim = serve_once(&path, |_| Some(Reply::Ok));
        // A launcher from a different binary: same protocol, other build.
        let mut stream = zest_daemon::connect(&path).expect("connect");
        let other = Hello { protocol: PROTOCOL, build: BuildId { version: "0.0.0".into(), exe_len: 1, exe_mtime: 1 } };
        write_frame(&mut stream, &other).unwrap();
        write_frame(&mut stream, &request()).unwrap();
        let mut reader = frame::FrameReader::new();
        let body = read_frame(&mut stream, &mut reader).unwrap().expect("an answer");
        assert_eq!(frame::decode::<Reply>(&body).unwrap(), Reply::OtherBuild);
    }

    /// The child of the two-process test: forward from a *separate process*,
    /// exit 0 on `Opened`. Only the cross-process form catches a pipe DACL
    /// or `FIRST_PIPE_INSTANCE` regression — threads share everything.
    #[test]
    #[ignore]
    fn a_stand_in_launcher() {
        let path = std::env::var("ZESTERM_INSTANCE_TEST_PATH").expect("the parent names the endpoint");
        let verdict = forward(&path, &request(), Duration::from_secs(5));
        assert_eq!(verdict, Verdict::Opened, "the other process did not answer");
    }

    #[test]
    fn a_second_process_reaches_a_server_in_this_one() {
        let path = test_path("proc");
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let saw = Arc::clone(&seen);
        let _claim = serve_once(&path, move |r| {
            *saw.lock() = Some(r);
            Some(Reply::Ok)
        });
        let me = std::env::current_exe().expect("a test binary knows its own path");
        let status = std::process::Command::new(me)
            .args(["--exact", "--ignored", "--test-threads=1", "instance::tests::a_stand_in_launcher"])
            .env("ZESTERM_INSTANCE_TEST_PATH", &path)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run the stand-in launcher");
        assert!(status.success(), "the launcher process did not get `Opened`: {status}");
        assert_eq!(seen.lock().as_ref().map(|r| r.command.clone()), Some(Some("htop".into())));
    }
}
