//! Signing this machine into an account, and the pairing prompt that follows.
//!
//! Moved out of `app/mod.rs` (#554) as code only -- `AccountState`, the
//! `Arc<Mutex<..>>` handoff cells and `App`'s fields stay on the parent, which
//! reads them from the Fleet screen and the chrome.
//!
//! The state machine stays per-window on purpose (ADR-018): it is driven by a
//! Fleet screen *inside* a window, and hoisting it onto `Shared` would put ~44
//! sites behind `RefCell` borrows across the longest methods in the crate, for
//! a limitation that costs nothing today.
//!
//! Every long-running step here is a spawned thread that parks its answer in a
//! cell and posts a wakeup, rather than blocking the event loop -- which is
//! also why `link_generation` exists: a reply from a link attempt the user has
//! since cancelled must not overwrite the state of the one they started after
//! it.

use super::*;

impl App {
    /// The account machinery starts on this machine's own word, not on a
    /// screen (#537): until the listing exists a relay-only host is
    /// unroutable, so the `+` menu shows no published profiles and the status
    /// bar undercounts — for the whole run, unless the person happens to open
    /// the Fleet screen. The loopback offer's `has_account_token` arrives
    /// keychain-free, and `Some(true)` is the signal that reading the stored
    /// token is finally worth it; `Some(false)`, `None` (an old daemon, a
    /// locked store) and a missing offer all keep the keychain untouched, so
    /// a loopback-only machine still never pays for it.
    pub(super) fn should_start_account_watch(&self, fleet: &[crate::fleet::FleetHost]) -> bool {
        self.account_poke.is_none() && local_daemon_is_enrolled(fleet)
    }

    /// Start the fleet's account watcher, once. The fetch closure is the
    /// whole transport: stored token → bearer GET /api/hosts → the minimal
    /// entries fleet.rs merges on. A 401 also flips the header through the
    /// account cell — the listing and the "signed in" line must not
    /// disagree about whether the token still works.
    pub(super) fn start_account_watch(&mut self) {
        if self.account_poke.is_some() {
            return;
        }
        // `--screen fleet` dispatches before the fleet model exists; the
        // resumed path calls back in once it does.
        let Some(fleet) = self.shared.fleet.get() else { return };
        let update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let poke = fleet.watch_account(move || {
            use crate::fleet::{AccountEntry, AccountError};
            let token = crate::cloud::stored_app_token(&zest_mesh::keystore::OsKeyStore)
                .map_err(|e| AccountError::Transient(e.to_string()))?
                .ok_or(AccountError::SignedOut)?;
            let api = crate::cloud::HttpsAccountApi::new(
                zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
                zest_cloud::tls::Roots::Platform,
            )
            .map_err(|e| AccountError::Transient(e.to_string()))?;
            // Hosts and devices in one fetch pass: both are the account's
            // word, and one refreshing while the other failed would draw a
            // fleet screen describing two different moments.
            let answer = crate::cloud::fetch_hosts(&api, &token).and_then(|listing| {
                let devices = crate::cloud::fetch_devices(&api, &token)?;
                Ok((listing, devices))
            });
            match answer {
                Ok((listing, devices)) => Ok(crate::fleet::AccountListing {
                    // The origin rides along: it is what turns an enrolled
                    // row into a routable card (`best_route`'s relay arm).
                    relay_origin: listing.relay_origin,
                    hosts: listing
                        .hosts
                        .into_iter()
                        .map(|h| AccountEntry {
                            host: h.host,
                            label: h.label,
                            // The fact #237 was about: dropping this here is
                            // what left `snapshot()` with nothing to say about
                            // a machine only the account knows.
                            relay_online: h.relay_online,
                        })
                        .collect(),
                    devices: devices
                        .into_iter()
                        .map(|d| crate::fleet::AccountDevice {
                            id: d.id,
                            approved: d.approved(),
                            label: d.label,
                            kind: d.kind,
                        })
                        .collect(),
                }),
                Err(crate::cloud::CloudError::SignedOut) => {
                    // The token was revoked out from under us. The watcher
                    // parks either way; this is what keeps the header from
                    // going on claiming "signed in" about a dead token.
                    post_account(&update, &proxy, AccountState::SignedOut);
                    Err(AccountError::SignedOut)
                }
                Err(crate::cloud::CloudError::Refused(why)) => {
                    // The 401 named its cause (#371): the header can say the
                    // person's actual next move instead of "not signed in".
                    use crate::cloud::MachineRefusal;
                    post_account(
                        &update,
                        &proxy,
                        match why {
                            MachineRefusal::Revoked => AccountState::Revoked,
                            MachineRefusal::Pending => AccountState::PendingApproval,
                            // An expired token is the one case where signing
                            // in again is genuinely the whole answer.
                            MachineRefusal::Expired => AccountState::SignedOut,
                        },
                    );
                    Err(AccountError::SignedOut)
                }
                Err(e) => Err(AccountError::Transient(e.to_string())),
            }
        });
        self.account_poke = Some(poke);
    }

    /// "Enroll this machine" (issue #227): mint a host code with the app's
    /// own token, carry it to the local daemon over a fresh short-lived
    /// loopback connection, surface how it went. All off the event loop —
    /// two HTTPS round trips and a keychain sit on this path.
    ///
    /// A fresh connection, `decide_pairing`'s shape and reason: the standing
    /// fleet-watch connection's thread is parked in `read`, and the daemon
    /// gates `Enroll` on the transport, so a fresh loopback dial carries
    /// exactly the same authority.
    pub(super) fn enroll_local_daemon(&mut self) {
        if matches!(self.local_enroll, LocalEnroll::InFlight) {
            return;
        }
        // The card only offers the button on a LocalSocket route, but a
        // click races a route change; re-check rather than unwrap.
        let Some(route @ HostRoute::LocalSocket(_)) = self.route.clone() else {
            return;
        };
        self.local_enroll = LocalEnroll::InFlight;
        self.mark_chrome_dirty();
        let update = Arc::clone(&self.local_enroll_update);
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("zest-enroll-local".into()).spawn(
            move || {
                let outcome = (|| -> Result<LocalEnroll, String> {
                    let token = crate::cloud::stored_app_token(&zest_mesh::keystore::OsKeyStore)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "signed out; sign in first".to_string())?;
                    let api = crate::cloud::HttpsAccountApi::new(
                        zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
                        zest_cloud::tls::Roots::Platform,
                    )
                    .map_err(|e| e.to_string())?;
                    let code =
                        crate::cloud::mint_host_code(&api, &token).map_err(|e| e.to_string())?;
                    let identity = zest_mesh::identity::ClientIdentity::generate()
                        .map(Arc::new)
                        .map_err(|e| e.to_string())?;
                    let (read, write) = (route.dialer())().map_err(|e| e.to_string())?;
                    let mut daemon = zest_daemon::client::DaemonClient::connect(
                        read,
                        write,
                        &identity,
                        "zesterm-enroll",
                        None,
                        false,
                    )
                    .map_err(|e| e.to_string())?;
                    match daemon.enroll(&code) {
                        Ok(done) if done.ok => Ok(LocalEnroll::Enrolled { account: done.account }),
                        // The daemon's own refusal, verbatim: it is phrased
                        // as the person's next move already.
                        Ok(done) => Err(done.message),
                        Err(e) => Err(enroll_failure_text(&e, &code)),
                    }
                })();
                let state = match outcome {
                    Ok(state) => state,
                    Err(message) => LocalEnroll::Failed(message),
                };
                *update.lock() = Some(state);
                let _ = proxy.send_event(Wakeup::AccountChanged);
            },
        );
        if let Err(e) = spawned {
            self.local_enroll =
                LocalEnroll::Failed(format!("no thread for the enrolment: {e}"));
            tracing::warn!(error = %e, "no thread for the local enrolment");
        }
    }

    /// Read the stored app token off the event loop and post what it means.
    pub(super) fn probe_account(&mut self) {
        let update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("zest-app-cloud".into()).spawn(move || {
            let state = probed_account_state(crate::cloud::stored_app_token(
                &zest_mesh::keystore::OsKeyStore,
            ));
            post_account(&update, &proxy, state);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no account probe; the fleet header stays unknown");
        }
    }

    /// Enrol this app with `code`, off the event loop, and post the outcome.
    pub(super) fn spawn_enroll(&mut self, code: String) {
        let identity = match self.durable_identity() {
            Ok(i) => i,
            Err(reason) => {
                self.account = AccountState::Failed(reason);
                self.mark_chrome_dirty();
                return;
            }
        };
        // A code sign-in supersedes any browser hand-off still polling.
        self.link_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.account = AccountState::Enrolling;
        let label = self.local_machine_label();
        let update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("zest-app-enroll".into()).spawn(move || {
            let state = match crate::cloud::enroll_desktop(
                &identity,
                &code,
                &label,
                zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
                &zest_daemon::enroll::HttpsControlPlane::new(zest_cloud::tls::Roots::Platform),
                &zest_mesh::keystore::OsKeyStore,
            ) {
                Ok(enrolled) => AccountState::SignedIn { account: enrolled.account },
                Err(e) => AccountState::Failed(enroll_failure(&e)),
            };
            post_account(&update, &proxy, state);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the enrolment worker");
            self.account = AccountState::Failed("could not start the enrolment worker".into());
        }
        self.mark_chrome_dirty();
    }

    /// Forget the app's token off the event loop and post the sign-out.
    /// The header keeps saying "signed in" until the worker settles — the
    /// delete is near-instant, and an "enrolling…" interim would be a lie.
    pub(super) fn spawn_sign_out(&mut self) {
        // Signing out also abandons any hand-off still polling.
        self.link_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("zest-app-cloud".into()).spawn(move || {
            if let Err(e) = crate::cloud::forget_app_token(&zest_mesh::keystore::OsKeyStore) {
                tracing::warn!(error = %e, "could not delete the app's cloud token");
            }
            // SignedOut either way: a delete that failed still means the app
            // should stop presenting the token, and the warn above is where
            // the store's trouble is named.
            post_account(&update, &proxy, AccountState::SignedOut);
        });
        if let Err(e) = spawned {
            // The enroll worker's shape: a spawn that failed must say so on
            // screen, or the header keeps claiming "signed in" about a
            // sign-out that never started.
            tracing::warn!(error = %e, "could not start the sign-out worker");
            self.account = AccountState::Failed("could not sign out — try again".into());
        }
        self.mark_chrome_dirty();
    }

    /// Start the browser hand-off (#226): ask for a grant, open the system
    /// browser at the approval page, and poll the claim until someone
    /// answers or the grant dies.
    ///
    /// The identity loads here, on the click (`spawn_enroll`'s keychain
    /// trade), and the throwaway fallback is refused — a device row bound
    /// to a key that evaporates on restart is the enrol rule restated. The
    /// header flips to `Linking` immediately, fingerprint included, so the
    /// person has the string to compare *before* the browser page appears.
    pub(super) fn spawn_link(&mut self) {
        let identity = match self.durable_identity() {
            Ok(i) => i,
            Err(reason) => {
                self.account = AccountState::Failed(reason);
                self.mark_chrome_dirty();
                return;
            }
        };
        // This hand-off supersedes any previous one still polling: the
        // server rotates the grant on the second start (one live grant per
        // key), so the old poller's grant is dead either way — the bump is
        // what keeps its last claim from overwriting this state.
        let generation = Arc::clone(&self.link_generation);
        let mine = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        self.account = AccountState::Linking {
            fingerprint: crate::cloud::key_fingerprint(identity.client_id()),
        };
        let label = self.local_machine_label();
        let update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("zest-app-link".into()).spawn(move || {
            use crate::cloud::LinkOutcome;
            let base = zest_daemon::enroll::DEFAULT_CONTROL_PLANE;
            let http =
                zest_daemon::enroll::HttpsControlPlane::new(zest_cloud::tls::Roots::Platform);
            let store = zest_mesh::keystore::OsKeyStore;

            // Posts only if this hand-off is still the current one — a
            // cancelled or superseded poller's outcome is nobody's news.
            let post = |state: AccountState| {
                if generation.load(std::sync::atomic::Ordering::SeqCst) == mine {
                    post_account(&update, &proxy, state);
                }
            };

            let granted = match crate::cloud::start_link(&identity, &label, base, &http, &store)
            {
                Ok(g) => g,
                Err(e) => {
                    post(AccountState::Failed(enroll_failure(&e)));
                    return;
                }
            };
            // The browser opens only after the grant exists — an approval
            // page for a grant that failed to mint is a 404 with no story.
            crate::platform::open_url(&format!(
                "{}{}?grant={}",
                base.trim_end_matches('/'),
                crate::cloud::LINK_PAGE_PATH,
                granted.grant,
            ));

            let expired = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64)
                    >= granted.expires_at
            };
            let gave_up = || {
                AccountState::Failed("the browser approval expired — try again".into())
            };

            loop {
                std::thread::sleep(LINK_POLL);
                if generation.load(std::sync::atomic::Ordering::SeqCst) != mine {
                    return;
                }
                // Expiry decides *before* the claim does. Checked only after,
                // a grant that died during the sleep still bought one more
                // claim, and the server's collapsed refusal reads "the
                // browser said no" — a refusal nobody made, about a page the
                // person may never have opened.
                if expired() {
                    post(gave_up());
                    return;
                }
                match crate::cloud::claim_link(&identity, &granted.grant, base, &http, &store) {
                    Ok(LinkOutcome::SignedIn { account }) => {
                        post(AccountState::SignedIn { account });
                        return;
                    }
                    Ok(LinkOutcome::Refused(message)) => {
                        // The same reading for the race the check above
                        // cannot close: the grant can die between it and the
                        // server's own read of the clock.
                        post(if expired() {
                            gave_up()
                        } else {
                            AccountState::Failed(format!("the browser said no: {message}"))
                        });
                        return;
                    }
                    // Pending keeps polling; a transport blip does too — the
                    // grant outlives a dropped packet, and giving up on one
                    // would fail hand-offs on exactly the flaky networks
                    // this flow exists to spare people typing codes on.
                    Ok(LinkOutcome::Pending) | Err(_) => {}
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the link worker");
            self.account = AccountState::Failed("could not start the sign-in worker".into());
        }
        self.mark_chrome_dirty();
    }

    /// Stop waiting on the browser. Local only, and honestly so: the grant
    /// lives its ten minutes out server-side, where an unclaimed approval
    /// enrols nobody — the claim signature is the thing that was cancelled.
    pub(super) fn cancel_link(&mut self) {
        self.link_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.account = AccountState::SignedOut;
        self.mark_chrome_dirty();
    }

    /// Approve or vouch for the devices-section row at `index`, off the
    /// event loop (#190: the app as approver).
    ///
    /// The row resolves against `devices_view` — the snapshot the section's
    /// indices were built from — and the identity loads *here*, on the
    /// click, which is the same keychain trade `spawn_enroll` makes. The
    /// worker then runs the whole ladder: token, `/api/me` for the userId
    /// the statement must name, sign, encode, POST — and pokes the account
    /// watcher on success so the listing refreshes with the row's new state.
    pub(super) fn spawn_approve(&mut self, index: usize) {
        let Some(device) = self.devices_view.get(index).cloned() else { return };
        let identity = match self.durable_identity() {
            Ok(i) => i,
            Err(reason) => {
                self.devices_error = Some(reason);
                self.mark_chrome_dirty();
                return;
            }
        };
        if identity.client_id() == device.id {
            // Reachable when the row was drawn before the keychain was ever
            // consulted (`fleet_device_rows`'s own-key note): refused here
            // with a name rather than shipped for the server's 400.
            self.devices_error =
                Some("this is this app's own key — another device must vouch for it".into());
            self.mark_chrome_dirty();
            return;
        }
        self.devices_error = None;
        let update = Arc::clone(&self.devices_error_update);
        let account_update = Arc::clone(&self.account_update);
        let proxy = self.proxy.clone();
        let poke = self.account_poke.clone();
        let spawned = std::thread::Builder::new().name("zest-app-approve".into()).spawn(
            move || {
                let outcome = approve_on_account(&identity, &device);
                match outcome {
                    Ok(()) => {
                        // The listing this approval changed is the watcher's
                        // to re-read; the poke is what makes the row flip
                        // now rather than a poll interval later.
                        if let Some(poke) = poke.as_ref() {
                            poke.poke();
                        }
                        *update.lock() = Some(None);
                    }
                    Err(ApproveFailure::SignedOut) => {
                        // The header must stop claiming otherwise, exactly
                        // as the account watcher does on its own 401s.
                        post_account(&account_update, &proxy, AccountState::SignedOut);
                        *update.lock() = Some(Some(
                            "signed out — sign in with a code before approving".into(),
                        ));
                    }
                    Err(ApproveFailure::Message(m)) => {
                        *update.lock() = Some(Some(m));
                    }
                }
                let _ = proxy.send_event(Wakeup::AccountChanged);
            },
        );
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the approval worker");
            self.devices_error = Some("could not start the approval worker".into());
        }
        self.mark_chrome_dirty();
    }

    /// The `AttachOptions::on_pending` callback for an attach headed at
    /// `host`: park the code in the shared cell and wake the event loop —
    /// the worker thread doing the waiting cannot touch the chrome itself.
    pub(super) fn pairing_notifier(&self, host: String) -> crate::remote::PendingCallback {
        let cell = Arc::clone(&self.pairing);
        let proxy = self.proxy.clone();
        Arc::new(move |code, expires_in_secs| {
            let proxy = proxy.clone();
            arm_pairing_prompt(
                &cell,
                host.clone(),
                code,
                expires_in_secs,
                Arc::new(move || {
                    let _ = proxy.send_event(Wakeup::PairingChanged);
                }),
            );
        })
    }

    /// The chrome's notice line, while an approval is pending and its code
    /// still worth comparing.
    /// The approval modal's content: the queue's visible request, while its
    /// code is still worth comparing. `None` when every entry is dismissed,
    /// expired, or resolved — the modal closes (or advances) by
    /// [`visible_approval`] moving on, which is what makes every close path
    /// one rule.
    pub(super) fn approval_model(&self) -> Option<crate::chrome::model::ApprovalModel> {
        let queue = self.approval.lock();
        let now = std::time::Instant::now();
        let r = &queue[visible_approval(&queue, now)?];
        let left = r.expires_at.saturating_duration_since(now);
        Some(crate::chrome::model::ApprovalModel {
            label: r.label.clone(),
            remote: r.remote.clone(),
            code: r.code.clone(),
            expires: format!("code expires in {}m", left.as_secs().div_ceil(60)),
        })
    }

    /// Answer the modal. The cell empties immediately — the person decided,
    /// and a modal that lingers while a socket round-trips invites a second
    /// click — and the daemon's tombstone push reconciles the optimistic
    /// close if the delivery fails (the request then still shows at the
    /// daemon's own prompt).
    pub(super) fn decide_approval(&mut self, approve: bool) {
        let taken = {
            let mut queue = self.approval.lock();
            // The entry the modal is showing — the same predicate the
            // drawing used, so a click can never answer for a request the
            // person was not looking at.
            visible_approval(&queue, std::time::Instant::now()).map(|i| queue.remove(i))
        };
        let Some(request) = taken else { return };
        if let Some(fleet) = self.shared.fleet.get() {
            fleet.decide_pairing(request.client, approve);
        } else {
            tracing::warn!("no fleet model; the pairing decision has nowhere to go");
        }
        self.mark_chrome_dirty();
    }

    pub(super) fn pairing_notice(&self) -> Option<String> {
        let cell = self.pairing.lock();
        let prompt = cell.as_ref()?;
        let left = prompt.expires_at.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            // The host has thrown the code away; showing it would invite
            // comparing a number that can no longer match anything.
            return None;
        }
        Some(format!(
            "waiting for approval on {} — code {} · {}m left",
            prompt.host,
            prompt.code,
            left.as_secs().div_ceil(60)
        ))
    }
}

#[cfg(test)]
mod enroll_tests {
    use super::enroll_failure_text;

    #[test]
    fn an_old_daemons_refusal_becomes_the_persons_next_move() {
        // An old daemon answers the unknown `Enroll` tag with `Error("could
        // not understand that message: …")` and keeps serving. Shown
        // verbatim that is true and useless; the mapping names the fallback
        // and carries the already-minted code, so the trip to the browser is
        // not wasted.
        let e = zest_daemon::DaemonError::Refused(
            "could not understand that message: unknown variant `enroll`".into(),
        );
        let text = enroll_failure_text(&e, "ABCD1234");
        assert!(
            text.contains("zest-daemon --enroll ABCD1234"),
            "the card must hand the person the exact command: {text}"
        );

        // Every other refusal keeps the daemon's own phrasing — it is
        // already the person's next move.
        let e = zest_daemon::DaemonError::Refused("only a local client may enroll this machine".into());
        assert!(
            enroll_failure_text(&e, "X").contains("only a local client"),
            "a real refusal must not be rewritten"
        );
    }

    #[test]
    fn already_enrolled_never_says_mint_a_fresh_one() {
        // The loop #368 kills: a revoked (or foreign-account) key hits the
        // same 409 with every code ever minted, so "mint a fresh one" sent
        // the person in circles. Every shape of the refusal — with either
        // detail or with none (a Worker predating #367) — must name a way
        // out instead.
        use super::enroll_failure;
        let refused = |detail: Option<&str>| zest_daemon::enroll::EnrollError::Refused {
            status: 409,
            message: "already_enrolled".into(),
            detail: detail.map(String::from),
        };

        let revoked = enroll_failure(&refused(Some("revoked")));
        assert!(
            revoked.contains("restore") && !revoked.contains("mint"),
            "revoked means restore, not another code: {revoked:?}"
        );
        let foreign = enroll_failure(&refused(Some("other_account")));
        assert!(
            foreign.contains("different account") && !foreign.contains("mint"),
            "a key on another account cannot be restored from this one: {foreign:?}"
        );
        let bare = enroll_failure(&refused(None));
        assert!(
            bare.contains("restore") && !bare.contains("mint"),
            "an old Worker names no cause, but a fresh code still cannot help: {bare:?}"
        );

        // And the collapsed dead-code/bad-signature refusal keeps its advice
        // — there a fresh code genuinely is the next move.
        let dead = enroll_failure(&zest_daemon::enroll::EnrollError::Refused {
            status: 400,
            message: "invalid_code".into(),
            detail: None,
        });
        assert!(dead.contains("mint a fresh one"), "got {dead:?}");
    }

    #[test]
    fn an_unreadable_keychain_is_not_rendered_as_signed_out() {
        // #371: "not signed in" about a fully-enrolled machine whose keychain
        // is merely locked is the lie that costs the diagnosis. The store's
        // failure is a fact about the store, and the state says so.
        use super::{probed_account_state, AccountState};

        let locked = probed_account_state(Err(zest_daemon::enroll::EnrollError::BadResponse(
            "the keychain is locked".into(),
        )));
        let AccountState::StoreUnreadable(message) = locked else {
            panic!("a store error must be its own state, got {locked:?}");
        };
        assert!(message.contains("keychain is locked"), "the store's own words: {message:?}");

        assert_eq!(
            probed_account_state(Ok(None)),
            AccountState::SignedOut,
            "only a store that answered 'nothing there' is signed out"
        );
        assert_eq!(
            probed_account_state(Ok(Some("zt1_x".into()))),
            AccountState::SignedIn { account: None }
        );
    }
}

#[cfg(test)]
mod account_watch_tests {
    use super::local_daemon_is_enrolled;

    fn with_token(local: bool, token: Option<bool>) -> Vec<crate::fleet::FleetHost> {
        let mut h = if local {
            zest_fleet::fixture::local(1, "studio")
        } else {
            zest_fleet::fixture::host(2, "forge")
        };
        h.offer = Some(zest_proto::HostOffer { has_account_token: token, ..Default::default() });
        vec![h]
    }

    #[test]
    fn the_account_watch_starts_on_the_local_daemons_word_alone() {
        // The gate for #537's fix: an enrolled machine's fleet must work
        // without a visit to the Fleet screen, and an unenrolled one must
        // still never pay a keychain read for a screen it never opens.
        assert!(
            local_daemon_is_enrolled(&with_token(true, Some(true))),
            "the loopback offer says enrolled, keychain-free — start"
        );
        assert!(
            !local_daemon_is_enrolled(&with_token(true, Some(false))),
            "a readable store holding nothing is a machine that never enrolled"
        );
        assert!(
            !local_daemon_is_enrolled(&with_token(true, None)),
            "an old daemon or a locked store did not say — no keychain read on a guess"
        );
        assert!(
            !local_daemon_is_enrolled(&with_token(false, Some(true))),
            "another machine's enrolment says nothing about this one's token"
        );
        assert!(
            !local_daemon_is_enrolled(&[zest_fleet::fixture::local(1, "studio")]),
            "no offer yet — the loopback watcher has not answered"
        );
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::{
        arm_approval_request, arm_pairing_prompt, visible_approval, ApprovalCell, PairingCell,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn wait_until(limit: Duration, f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn an_expired_prompt_clears_itself_and_wakes_the_ui() {
        // The review finding on #208 this clock exists for: the chrome
        // snapshots the prompt into a *cached* layout, so once painted
        // nothing re-rendered the countdown or removed an expired code —
        // it could sit on screen indefinitely unless an unrelated event
        // happened to invalidate the chrome. Expiry must clear the cell and
        // wake the UI with no outside help.
        let cell: PairingCell = Arc::new(parking_lot::Mutex::new(None));
        let woken = Arc::new(AtomicUsize::new(0));
        let posts = Arc::clone(&woken);
        arm_pairing_prompt(
            &cell,
            "forge".into(),
            "481502".into(),
            1,
            Arc::new(move || {
                posts.fetch_add(1, Ordering::Release);
            }),
        );
        assert!(
            cell.lock().as_ref().is_some_and(|p| p.code == "481502"),
            "arming must store the prompt for the chrome to read"
        );
        assert!(woken.load(Ordering::Acquire) >= 1, "arming must wake the UI for the first paint");

        assert!(
            wait_until(Duration::from_secs(10), || cell.lock().is_none()),
            "the expired prompt never removed itself — a dead code stays painted \
             until some unrelated event rebuilds the chrome, which is the bug"
        );
        assert!(
            woken.load(Ordering::Acquire) >= 2,
            "clearing without a wake leaves the cached chrome still showing the \
             code; the removal must post too"
        );
    }

    #[test]
    fn a_replaced_prompt_is_not_clobbered_by_the_old_clock() {
        // A redial stores a fresh code while the old prompt's clock is still
        // sleeping. When that clock fires it must recognise the cell no
        // longer holds its prompt and go quietly — clearing here would
        // delete the *live* code out from under the person reading it.
        let cell: PairingCell = Arc::new(parking_lot::Mutex::new(None));
        let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        arm_pairing_prompt(&cell, "forge".into(), "111111".into(), 1, Arc::clone(&noop));
        arm_pairing_prompt(&cell, "forge".into(), "222222".into(), 120, noop);

        // Outlive the first prompt's expiry with margin: its clock has fired
        // and exited by now, or it was going to clobber us.
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            cell.lock().as_ref().is_some_and(|p| p.code == "222222"),
            "the old clock took the new prompt with it; the person is now \
             comparing a code the window no longer shows"
        );
    }

    /// Queue a request with a device id, a code, and minutes of validity.
    fn approval(cell: &ApprovalCell, id: u8, code: &str, secs: u32) {
        arm_approval_request(
            cell,
            zest_proto::ClientId::from_bytes([id; 32]),
            format!("device-{id}"),
            "192.168.1.42:60123".into(),
            code.into(),
            secs,
            Arc::new(|| {}),
        );
    }

    #[test]
    fn two_concurrent_requests_queue_and_both_can_be_answered() {
        // The review finding on #222: a single `Option` slot meant the
        // second device overwrote the first, and since the daemon announces
        // each device exactly once, the overwritten request could never be
        // answered from the modal at all. Both must survive; the modal
        // shows the older, and answering it advances to the newer.
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        approval(&cell, 0xd0, "111111", 120);
        approval(&cell, 0xd1, "222222", 120);

        let queue = cell.lock();
        assert_eq!(queue.len(), 2, "the second device must not overwrite the first");
        let visible = visible_approval(&queue, Instant::now()).expect("one shows");
        assert_eq!(queue[visible].code, "111111", "the modal shows arrivals in order");
        drop(queue);

        // Deciding removes exactly the visible entry (the decide path), and
        // the queue advances to the other device.
        let mut queue = cell.lock();
        let i = visible_approval(&queue, Instant::now()).expect("still one");
        let answered = queue.remove(i);
        assert_eq!(answered.code, "111111");
        let next = visible_approval(&queue, Instant::now()).expect("the second advances");
        assert_eq!(
            queue[next].code, "222222",
            "the request that used to be overwritten is now answerable"
        );
    }

    #[test]
    fn a_tombstone_for_the_visible_request_advances_to_the_next() {
        // Someone answered the visible device at the daemon's stdin: its
        // tombstone removes it here (the listener's retain), and the next
        // device's prompt shows instead of a dead one.
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        approval(&cell, 0xd0, "111111", 120);
        approval(&cell, 0xd1, "222222", 120);

        let mut queue = cell.lock();
        let gone = zest_proto::ClientId::from_bytes([0xd0; 32]);
        queue.retain(|r| r.client != gone);
        let next = visible_approval(&queue, Instant::now()).expect("the next shows");
        assert_eq!(queue[next].code, "222222");
    }

    #[test]
    fn dismissing_the_visible_request_shows_the_next() {
        // Esc is "not now", per request: the dismissed entry stays for its
        // tombstone but stops drawing, and the queue moves on.
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        approval(&cell, 0xd0, "111111", 120);
        approval(&cell, 0xd1, "222222", 120);

        let mut queue = cell.lock();
        let i = visible_approval(&queue, Instant::now()).expect("one shows");
        queue[i].dismissed = true;
        let next = visible_approval(&queue, Instant::now()).expect("the next shows");
        assert_eq!(queue[next].code, "222222", "dismiss hides one prompt, not the queue");
        queue[next].dismissed = true;
        assert!(
            visible_approval(&queue, Instant::now()).is_none(),
            "everything dismissed means no modal, not the first one back"
        );
    }

    #[test]
    fn an_expired_approval_request_clears_itself_and_the_next_shows() {
        // The inbound queue rides the same clock as #208's outbound cell,
        // and must: the modal is chrome, the chrome is cached, and a request
        // whose device long since gave up would otherwise sit on screen
        // asking a question that can no longer be answered — now with a
        // second device queued behind it, whose turn expiry must grant.
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let woken = Arc::new(AtomicUsize::new(0));
        let posts = Arc::clone(&woken);
        arm_approval_request(
            &cell,
            zest_proto::ClientId::from_bytes([0xd0; 32]),
            "andy-phone".into(),
            "192.168.1.42:60123".into(),
            "481502".into(),
            1,
            Arc::new(move || {
                posts.fetch_add(1, Ordering::Release);
            }),
        );
        approval(&cell, 0xd1, "222222", 120);
        assert!(
            wait_until(Duration::from_secs(10), || {
                cell.lock().iter().all(|r| r.code != "481502")
            }),
            "the expired request never removed itself — the modal would ask \
             for ever about a device that already gave up"
        );
        assert!(
            woken.load(Ordering::Acquire) >= 2,
            "the removal must wake the UI too, or the cached chrome keeps \
             the modal painted"
        );
        let queue = cell.lock();
        let next = visible_approval(&queue, Instant::now())
            .expect("expiry must hand the modal to the device still waiting");
        assert_eq!(queue[next].code, "222222");
    }

    #[test]
    fn a_rearmed_device_survives_its_old_requests_clock() {
        // A device that asks again replaces its own entry with a fresh code
        // and a fresh expiry. The replaced entry's clock, firing later, must
        // find nothing — clearing the newcomer would delete the code the
        // person is actively comparing (the same generation discipline as
        // the outbound prompt's clock).
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        approval(&cell, 0xd0, "111111", 1);
        approval(&cell, 0xd0, "222222", 120);
        assert_eq!(cell.lock().len(), 1, "one device is one prompt, not two");

        // Outlive the first entry's expiry with margin: its clock has fired
        // and exited by now, or it was going to clobber the replacement.
        std::thread::sleep(Duration::from_secs(2));
        let queue = cell.lock();
        assert!(
            queue.iter().any(|r| r.code == "222222"),
            "the old clock took the replacement with it"
        );
    }

    #[test]
    fn an_unknown_expiry_assumes_the_pairing_window_rather_than_never_closing() {
        // `expires_in_secs: 0` is an older daemon saying "field unknown".
        // The modal must still get a deadline — the daemon's own approval
        // window — because a modal with no deadline never self-clears.
        let cell: ApprovalCell = Arc::new(parking_lot::Mutex::new(Vec::new()));
        approval(&cell, 0xd1, "111111", 0);
        let deadline = cell.lock().first().map(|r| r.expires_at).expect("stored");
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            left > Duration::from_secs(30),
            "an unknown expiry must not read as already-expired (got {left:?})"
        );
        assert!(
            left <= zest_mesh::pairing::APPROVAL_TIMEOUT,
            "…and must not outlive the daemon's own window (got {left:?})"
        );
    }
}
