//! Main loop: reload accounts, poll each one, back off per account on
//! failure, exit cleanly on SIGTERM. Port of `worker.py`; the real mailbox
//! is `imap::ImapSource` (tests inject fakes through `source_factory`).

use crate::accounts::{Account, AccountSource};
use crate::catalog::PgCatalog;
use crate::classifier::StopEvent;
use crate::config::Settings;
use crate::imap::ImapSource;
use crate::ingest::{sync_mailbox, MailSource, SyncOptions};
use crate::logs::{LogValue, Logger};
use crate::signals;
use crate::storage::{S3Client, Store};
use postgres::Client;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// What `ImapSource::connect` raises for connect/login failures, boxed like
/// the arbitrary exceptions the Python factory may raise.
pub type SourceFactory<'a> =
    Box<dyn FnMut(&Account) -> Result<Box<dyn MailSource>, Box<dyn std::error::Error>> + 'a>;

struct AccountState {
    account: Account,
    failures: u32,
    next_try: f64,
}

pub struct Worker<'a, C: S3Client> {
    pub settings: Settings,
    pub store: &'a Store<C>,
    /// The Python catalog owns the pool; here the connection is passed per pass.
    pub catalog: &'a mut PgCatalog,
    pub accounts: &'a mut dyn AccountSource,
    pub source_factory: Option<SourceFactory<'a>>,
    pub stop: StopEvent,
    /// accounts that failed in the last `run_once`
    pub failed_accounts: Vec<String>,
    clock: Box<dyn Fn() -> f64 + 'a>,
    log: Logger,
    state: HashMap<String, AccountState>,
    warned_empty: bool,
    synced: HashSet<String>,
    /// `sync_mailbox` checks an AtomicBool; kept in step with `stop`.
    ingest_stop: Arc<AtomicBool>,
}

fn monotonic() -> f64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

impl<'a, C: S3Client> Worker<'a, C> {
    pub fn new(
        settings: Settings,
        store: &'a Store<C>,
        catalog: &'a mut PgCatalog,
        accounts: &'a mut dyn AccountSource,
        log: Logger,
    ) -> Worker<'a, C> {
        Worker {
            settings,
            store,
            catalog,
            accounts,
            source_factory: None,
            stop: StopEvent::new(),
            failed_accounts: Vec::new(),
            clock: Box::new(monotonic),
            log,
            state: HashMap::new(),
            warned_empty: false,
            synced: HashSet::new(),
            ingest_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Replace the monotonic clock (tests).
    pub fn set_clock(&mut self, clock: Box<dyn Fn() -> f64 + 'a>) {
        self.clock = clock;
    }

    /// The users currently being polled, sorted (for tests; Python's suite
    /// reads the private `_state`).
    pub fn tracked_users(&self) -> Vec<String> {
        let mut users: Vec<String> = self.state.keys().cloned().collect();
        users.sort();
        users
    }

    /// Catch SIGTERM/SIGINT and turn them into the stop event, so `run_once`
    /// breaks between accounts and `sync_mailbox` between batches.
    pub fn install_signal_handlers(&mut self) {
        signals::install();
        let _ = signals::spawn_watcher(&self.stop, self.ingest_stop.clone());
    }

    fn refresh(&mut self, conn: &mut Client) -> Vec<Account> {
        // dedup by user, keeping the first position (Python's dict)
        let loaded = self.accounts.load();
        let mut seen = HashSet::new();
        let current: Vec<Account> =
            loaded.into_iter().filter(|a| seen.insert(a.user.clone())).collect();

        let users: Vec<String> = current.iter().map(|a| a.user.clone()).collect();
        let new_users: Vec<String> =
            users.iter().filter(|u| !self.synced.contains(*u)).cloned().collect();
        if !new_users.is_empty() {
            let mut sorted = new_users.clone();
            sorted.sort();
            // each account's sync will fail and back off on its own
            match self.catalog.sync_accounts(conn, &sorted) {
                Ok(()) => self.synced = users.iter().cloned().collect(),
                Err(e) => self.log.error("cannot register accounts in the database", &[
                    ("error", LogValue::str(e.to_string())),
                ]),
            }
        }

        let current_users: HashSet<String> = users.into_iter().collect();
        let removed: Vec<String> = self
            .state
            .keys()
            .filter(|u| !current_users.contains(*u))
            .cloned()
            .collect();
        for user in removed {
            self.log.info("account removed or disabled", &[("account", LogValue::str(user.clone()))]);
            self.state.remove(&user);
        }
        for acct in &current {
            match self.state.get(&acct.user) {
                None => {
                    self.log.info("account added", &[("account", LogValue::str(acct.user.clone()))]);
                    self.state.insert(
                        acct.user.clone(),
                        AccountState { account: acct.clone(), failures: 0, next_try: 0.0 },
                    );
                }
                Some(st) if st.account != *acct => {
                    // New password, host or mailboxes: forget old failures and try right away.
                    self.log.info("account changed", &[("account", LogValue::str(acct.user.clone()))]);
                    self.state.insert(
                        acct.user.clone(),
                        AccountState { account: acct.clone(), failures: 0, next_try: 0.0 },
                    );
                }
                Some(_) => {}
            }
        }
        if current.is_empty() && !self.warned_empty {
            self.log.warning(
                "no accounts configured; waiting (add one with `aimap accounts add`)",
                &[],
            );
        }
        self.warned_empty = current.is_empty();
        current
    }

    /// One pass over every account. Returns the number of messages stored.
    pub fn run_once(&mut self, conn: &mut Client) -> usize {
        let mut total = 0;
        self.failed_accounts.clear();
        let accounts = self.refresh(conn);
        for acct in accounts {
            if self.stop.is_set() {
                break;
            }
            let next_try = self.state.get(&acct.user).map(|st| st.next_try).unwrap_or(0.0);
            if (self.clock)() < next_try {
                continue;
            }
            match self.sync_account(&acct, conn) {
                Ok(stored) => {
                    total += stored;
                    let st = self.state.get_mut(&acct.user).unwrap();
                    if st.failures > 0 {
                        self.log.info("account recovered", &[("account", LogValue::str(acct.user.clone()))]);
                    }
                    st.failures = 0;
                    st.next_try = 0.0;
                }
                // one broken account must not stop the others
                Err(e) => {
                    let st = self.state.get_mut(&acct.user).unwrap();
                    st.failures += 1;
                    self.failed_accounts.push(acct.user.clone());
                    let delay = (self.settings.max_backoff)
                        .min(self.settings.poll_interval * 2f64.powi(st.failures as i32 - 1));
                    st.next_try = (self.clock)() + delay;
                    self.log.error("account sync failed", &[
                        ("account", LogValue::str(acct.user.clone())),
                        ("error", LogValue::str(e.to_string())),
                        ("failures", LogValue::Int(st.failures as i64)),
                        ("retry_in_s", LogValue::Int(delay.round() as i64)),
                    ]);
                }
            }
        }
        total
    }

    fn sync_account(
        &mut self,
        acct: &Account,
        conn: &mut Client,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let mut source: Box<dyn MailSource> = match &mut self.source_factory {
            Some(factory) => factory(acct)?,
            None => Box::new(ImapSource::connect(acct, Duration::from_secs_f64(self.settings.imap_timeout))?),
        };
        let mut stored = 0;
        for mailbox in acct.mailboxes.clone() {
            if self.stop.is_set() {
                break;
            }
            let options = SyncOptions {
                initial_fetch_count: self.settings.initial_fetch_count as usize,
                batch_size: self.settings.batch_size,
                stop: Some(&self.ingest_stop),
            };
            let result = sync_mailbox(
                &mut *source,
                self.store,
                self.catalog,
                conn,
                &acct.user,
                &mailbox,
                &options,
            )?;
            stored += result.stored;
        }
        source.close();
        Ok(stored)
    }

    pub fn run_forever(&mut self, conn: &mut Client) {
        self.log.info("worker started", &[
            ("accounts_source", LogValue::str(self.settings.accounts_source.clone())),
            ("poll_interval_s", LogValue::Float(self.settings.poll_interval)),
            ("bucket", LogValue::str(self.store.cfg.bucket.clone())),
        ]);
        while !self.stop.is_set() {
            let started = (self.clock)();
            self.run_once(conn);
            let elapsed = (self.clock)() - started;
            let remaining = self.settings.poll_interval - elapsed;
            if remaining > 0.0 && self.stop.wait_timeout(Duration::from_secs_f64(remaining)) {
                break;
            }
        }
        if signals::received() {
            self.log.info("shutdown requested", &[("signal", LogValue::str(signals::last_name()))]);
        }
        self.log.info("worker stopped", &[]);
    }
}
