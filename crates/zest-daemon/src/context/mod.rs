//! What a session is standing in, computed where the session lives.
//!
//! The daemon answers "which branch, which venv, which cluster" so that every
//! client — the window, a browser on another continent, an agent over MCP —
//! renders the same chips without any of them running a command in the user's
//! shell. Everything here is *display* (`SessionContext`'s doc has the trust
//! argument); everything here is also *cheap*: file reads on a per-cwd cache,
//! invalidated by `notify` watchers, and never a subprocess. The facts that
//! need one (`dirty`, real runtime versions) stay unreported until an async
//! probe exists, because an honest `None` beats a stalled listing.
//!
//! Lazy on purpose: nothing is probed until a listing asks
//! ([`ContextEngine::context_for`]), so a daemon whose clients never look pays
//! nothing, and the probe runs on the connection's thread with the freshest
//! cwd rather than racing the shell from the reader's.

pub mod git;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zest_proto::{ContextFact, ContextSource, GitContext, SessionContext};

/// Cached probe results for this many distinct directories. Sessions occupy a
/// handful of cwds; the cap is for the pathological `cd` loop, not for use.
const MAX_CACHED_CWDS: usize = 64;
/// Watch this many distinct repositories' HEADs. Beyond it the oldest watch is
/// dropped — its sessions fall back to probe-on-list, which is stale-until-
/// something-else-moves rather than wrong.
const MAX_WATCHED_REPOS: usize = 16;

/// The per-daemon context cache, shared by every connection's listings.
pub struct ContextEngine {
    inner: Arc<Inner>,
}

struct Inner {
    /// Announces "a listing would read differently now" — the registry's
    /// coalesced touch. A callback rather than the registry itself, so the
    /// engine is testable with a counter.
    on_change: Arc<dyn Fn() + Send + Sync>,
    /// Bumped on every invalidation; snapshotted into each
    /// [`SessionContext::revision`] so clients can skip unchanged chrome.
    revision: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    by_cwd: HashMap<String, Probed>,
    /// Live HEAD watchers, keyed by the resolved HEAD path, valued with the
    /// insertion order that decides eviction.
    repos: HashMap<PathBuf, RepoWatch>,
    next_repo: u64,
    kube: Option<KubeState>,
}

/// What one directory probed to.
struct Probed {
    git: Option<GitContext>,
    /// The HEAD the git context came from — the invalidation key.
    head: Option<PathBuf>,
    /// Version-pin facts found walking up from the cwd (`.nvmrc`, …).
    pins: Vec<ContextFact>,
}

struct RepoWatch {
    _watcher: Option<zest_config::Watcher>,
    order: u64,
}

struct KubeState {
    current: Option<String>,
    _watcher: Option<zest_config::Watcher>,
}

impl ContextEngine {
    pub fn new(on_change: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                on_change,
                revision: AtomicU64::new(1),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// The context for a session whose shell reports `cwd` — `None` when
    /// there is nothing to say, which is every session until shell
    /// integration reports a directory.
    ///
    /// `host` is OSC 7's authority part when the shell sent one. Any host at
    /// all suppresses the filesystem probes: the path is not on this machine,
    /// and a branch read from a same-named local directory would be a chip
    /// confidently describing the wrong computer. The host becomes a fact
    /// instead — labeled `ShellReport`, because that is who said it.
    #[must_use]
    pub fn context_for(&self, cwd: &str, host: Option<&str>) -> Option<SessionContext> {
        if cwd.is_empty() {
            return None;
        }
        if let Some(host) = host {
            return Some(SessionContext {
                git: None,
                facts: vec![ContextFact {
                    key: "ssh_host".into(),
                    value: host.to_string(),
                    source: ContextSource::ShellReport,
                }],
                revision: self.inner.revision.load(Ordering::Acquire),
            });
        }

        let mut state = self.inner.state.lock().expect("context lock");
        // Read *under* the state lock: loaded earlier, an invalidation racing
        // in between would pair freshly-probed facts with the revision from
        // before it — and a client comparing revisions would skip rebuilding
        // chrome for data that did change.
        let revision = self.inner.revision.load(Ordering::Acquire);
        self.ensure_kube(&mut state);
        if !state.by_cwd.contains_key(cwd) {
            let probed = self.probe(&mut state, Path::new(cwd));
            if state.by_cwd.len() >= MAX_CACHED_CWDS {
                state.by_cwd.clear();
            }
            state.by_cwd.insert(cwd.to_string(), probed);
        }
        let probed = state.by_cwd.get(cwd).expect("just inserted");

        let mut facts = probed.pins.clone();
        if let Some(kube) = state.kube.as_ref().and_then(|k| k.current.clone()) {
            facts.push(ContextFact {
                key: "kube".into(),
                value: kube,
                source: ContextSource::DaemonProbe,
            });
        }
        Some(SessionContext { git: probed.git.clone(), facts, revision })
    }

    /// Probe one directory, and make sure its repository is being watched.
    fn probe(&self, state: &mut State, dir: &Path) -> Probed {
        let found = git::probe(dir);
        let (git_ctx, head) = match found {
            Some(p) => (Some(p.context), Some(p.head)),
            None => (None, None),
        };
        if let Some(head) = &head {
            self.watch_repo(state, head);
        }
        Probed { git: git_ctx, head, pins: pin_facts(dir) }
    }

    fn watch_repo(&self, state: &mut State, head: &Path) {
        if state.repos.contains_key(head) {
            return;
        }
        if state.repos.len() >= MAX_WATCHED_REPOS {
            if let Some(oldest) =
                state.repos.iter().min_by_key(|(_, w)| w.order).map(|(p, _)| p.clone())
            {
                state.repos.remove(&oldest);
                // Its cache entries go with it: an entry with no watcher is
                // not "stale until the next change", it is stale forever —
                // nothing would ever invalidate it. Dropping them degrades
                // the evicted repo to probe-on-list, as the cap promises.
                state.by_cwd.retain(|_, p| p.head.as_deref() != Some(oldest.as_path()));
            }
        }
        let inner = Arc::clone(&self.inner);
        let key = head.to_path_buf();
        // A failed watch degrades to probe-on-list, never to an error: the
        // listing still answers, it just answers stale until the next miss.
        let watcher = zest_config::Watcher::new(head, move || inner.head_changed(&key)).ok();
        let order = state.next_repo;
        state.next_repo += 1;
        state.repos.insert(head.to_path_buf(), RepoWatch { _watcher: watcher, order });
    }

    /// Read the kube context once and keep it fresh off its own watcher.
    ///
    /// Until the kubeconfig *exists* nothing is stored, so every listing
    /// retries with one `stat` — a config created after the daemon started
    /// shows up on the next ask instead of never. Watching only a file that
    /// exists also matters for its own sake: `Watcher::new` creates the
    /// parent directory, and a daemon conjuring `~/.kube` out of nothing is
    /// a side effect nobody asked a *listing* for.
    fn ensure_kube(&self, state: &mut State) {
        if state.kube.is_some() {
            return;
        }
        let Some(path) = kubeconfig_path().filter(|p| p.exists()) else { return };
        let current = kube_current_context(&path);
        let watcher = {
            let inner = Arc::clone(&self.inner);
            let for_change = path.clone();
            zest_config::Watcher::new(&path, move || inner.kube_changed(&for_change)).ok()
        };
        state.kube = Some(KubeState { current, _watcher: watcher });
    }
}

impl Inner {
    /// A repository's HEAD moved: forget what was probed through it.
    fn head_changed(&self, head: &Path) {
        {
            let mut state = self.state.lock().expect("context lock");
            state.by_cwd.retain(|_, p| p.head.as_deref() != Some(head));
        }
        self.revision.fetch_add(1, Ordering::Release);
        (self.on_change)();
    }

    fn kube_changed(&self, path: &Path) {
        {
            let mut state = self.state.lock().expect("context lock");
            if let Some(kube) = state.kube.as_mut() {
                kube.current = kube_current_context(path);
            }
        }
        self.revision.fetch_add(1, Ordering::Release);
        (self.on_change)();
    }
}

/// Version pins found walking up from `dir` — the *asked-for* runtime, which
/// a file states, as opposed to the *installed* one, which only a subprocess
/// knows and which therefore waits for the async probe.
fn pin_facts(dir: &Path) -> Vec<ContextFact> {
    const PINS: [(&str, &str); 3] =
        [("node", ".nvmrc"), ("python", ".python-version"), ("rust", "rust-toolchain.toml")];
    let mut facts = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        for (key, file) in PINS {
            if facts.iter().any(|f: &ContextFact| f.key == key) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(d.join(file)) else { continue };
            let value = match key {
                // `channel = "1.79"`, somewhere in the toml.
                "rust" => text
                    .lines()
                    .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == "channel"))
                    .map(|(_, v)| v.trim().trim_matches('"').to_string()),
                _ => text.lines().next().map(|l| l.trim().to_string()),
            };
            if let Some(value) = value.filter(|v| !v.is_empty() && v.len() <= 64) {
                facts.push(ContextFact { key: key.into(), value, source: ContextSource::DaemonProbe });
            }
        }
        cur = d.parent();
    }
    facts
}

/// `$KUBECONFIG`'s first entry, else `~/.kube/config`.
fn kubeconfig_path() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("KUBECONFIG") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        if let Some(first) = env.split(sep).find(|p| !p.is_empty()) {
            return Some(PathBuf::from(first));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".kube").join("config"))
}

/// The `current-context:` line of a kubeconfig — a line scan, not a YAML
/// parser: the field is top-level and single-line in every kubeconfig kubectl
/// itself writes, and a daemon is not where a YAML dependency earns its keep.
fn kube_current_context(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let value = line.strip_prefix("current-context:")?.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn engine() -> (ContextEngine, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        let engine = ContextEngine::new(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        (engine, hits)
    }

    #[test]
    fn an_empty_cwd_is_no_context_at_all() {
        let (engine, _) = engine();
        assert!(
            engine.context_for("", None).is_none(),
            "a session that never reported a cwd has nothing to say, not an empty something"
        );
    }

    #[test]
    fn a_remote_cwd_probes_nothing_and_names_the_host() {
        let (engine, _) = engine();
        // This path exists locally and is a git repo -- which is exactly the
        // trap: the host part must keep the probe away from it.
        let here = env!("CARGO_MANIFEST_DIR");
        let ctx = engine.context_for(here, Some("build-box")).expect("a context");
        assert!(ctx.git.is_none(), "a local repo must not describe another machine's path");
        assert_eq!(ctx.facts.len(), 1);
        assert_eq!(ctx.facts[0].key, "ssh_host");
        assert_eq!(ctx.facts[0].value, "build-box");
        assert!(
            matches!(ctx.facts[0].source, ContextSource::ShellReport),
            "the shell said the host; the label has to say the shell said it"
        );
    }

    #[test]
    fn a_local_repo_cwd_reports_its_branch() {
        let (engine, _) = engine();
        let ctx = engine.context_for(env!("CARGO_MANIFEST_DIR"), None).expect("a context");
        let git = ctx.git.expect("this test runs inside zesterm's own checkout");
        assert!(!git.branch.is_empty());
        assert_eq!(git.dirty, None, "no subprocess ran, so nothing may claim clean or dirty");
    }

    #[test]
    fn a_head_change_invalidates_and_announces() {
        let (engine, hits) = engine();
        let root = std::env::temp_dir().join(format!("zest-ctx-inval-{}", std::process::id()));
        let git = root.join(".git");
        std::fs::create_dir_all(&git).expect("mkdir");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/first\n").expect("write");
        let cwd = root.to_string_lossy().to_string();

        let before = engine.context_for(&cwd, None).expect("a context");
        assert_eq!(before.git.as_ref().expect("a repo").branch, "first");

        // The cache, not the filesystem, answers the second ask.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/second\n").expect("write");
        let cached = engine.context_for(&cwd, None).expect("a context");
        assert_eq!(
            cached.git.as_ref().expect("a repo").branch,
            "first",
            "the second ask must be a cache hit -- if this reads 'second' the cache is not caching"
        );

        // The watcher notices, invalidates, and announces; the next ask
        // re-probes. Generous deadline for the same reason watch.rs gives.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let now = engine.context_for(&cwd, None).expect("a context");
            if now.git.as_ref().expect("a repo").branch == "second" {
                assert!(now.revision > before.revision, "invalidation must move the revision");
                assert!(hits.load(Ordering::SeqCst) >= 1, "nobody was told the listing changed");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the HEAD change was never noticed; the watcher is not watching"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn version_pins_are_found_and_labeled_as_probes() {
        let root = std::env::temp_dir().join(format!("zest-ctx-pins-{}", std::process::id()));
        let deep = root.join("packages").join("app");
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(root.join(".nvmrc"), "20.11.1\n").expect("write");
        std::fs::write(root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.79\"\n")
            .expect("write");

        let facts = pin_facts(&deep);
        let node = facts.iter().find(|f| f.key == "node").expect("the walk reaches the root");
        assert_eq!(node.value, "20.11.1");
        let rust = facts.iter().find(|f| f.key == "rust").expect("toml channel parsed");
        assert_eq!(rust.value, "1.79");
        assert!(facts.iter().all(|f| matches!(f.source, ContextSource::DaemonProbe)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kubeconfig_current_context_is_a_line_scan() {
        let path = std::env::temp_dir().join(format!("zest-ctx-kube-{}", std::process::id()));
        std::fs::write(&path, "apiVersion: v1\ncurrent-context: prod-eu\nclusters: []\n")
            .expect("write");
        assert_eq!(kube_current_context(&path).as_deref(), Some("prod-eu"));
        let _ = std::fs::remove_file(&path);
    }
}
