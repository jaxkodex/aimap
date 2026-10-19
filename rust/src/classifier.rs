//! Port of `src/aimap/classifier.py`: the `aimap classify` process — take
//! classify jobs, ask Jev, store the labels.
//!
//! For each job:
//!
//! 1. Load the message metadata, its account's profile and the profile's
//!    patterns.
//! 2. Build the questions and the classifier key. If this message already has
//!    a classification with that key, mark the job done without calling Jev.
//! 3. Read the raw message from S3 and build the Jev state (header facts plus
//!    a trimmed body, in memory only).
//! 4. Call Jev, run [`jev::decide`], then insert the classification and mark
//!    the job done in one transaction.
//!
//! `concurrency` threads each claim their own jobs. The main thread puts back
//! jobs left `running` by a worker that died, and stops everything on
//! shutdown (the `stop` event; signal handlers arrive with the worker port).
//!
//! The model call lives behind [`Asker`] (Python's `Asker` protocol), so
//! tests fake Jev exactly like the Python suite does. The production
//! TypeSafe transport is not ported yet (the `typesafe-sdk` is closed to this
//! sandbox; the Python suite never tests it either).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;


use serde_json::{Map, Value};

use crate::config::ClassifierSettings;
use crate::db::Pool;
use crate::jev;
use crate::parse::MessageMeta;
use crate::profiles;
use crate::queue::{self, Job};
use crate::storage::{S3Client, Store, StoreError};

pub const STAGE: &str = "classify";

/// threading.Event, for the stop flag.
#[derive(Clone, Default)]
pub struct StopEvent(Arc<(Mutex<bool>, Condvar)>);

impl StopEvent {
    pub fn new() -> StopEvent {
        StopEvent::default()
    }

    pub fn is_set(&self) -> bool {
        *self.0 .0.lock().unwrap()
    }

    pub fn set(&self) {
        let mut set = self.0 .0.lock().unwrap();
        *set = true;
        self.0 .1.notify_all();
    }

    /// Wait up to `timeout` for the flag; true once it is set
    /// (`Event.wait(timeout)` in Python).
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (lock, cond) = &*self.0;
        let (set, _) = cond
            .wait_timeout_while(lock.lock().unwrap(), timeout, |set| !*set)
            .unwrap();
        *set
    }
}

/// Jev's answer for one message.
#[derive(Debug, Clone, PartialEq)]
pub struct JevResult {
    pub model: String,
    pub answers: Map<String, Value>,
    pub usage: Option<Value>,
}

/// The `Asker` protocol: one model call per classifier state.
pub trait Asker: Send {
    fn ask(&mut self, state: &Value, questions: &Value) -> Result<JevResult, AskError>;
}

#[derive(Debug)]
pub enum AskError {
    /// `PermanentError`: retrying will not help.
    Permanent(String),
    /// API/network errors: retried through the queue, logged without a
    /// traceback (`TypeSafeError | OSError` in Python).
    Api(String),
}

/// What can go wrong inside one job. `Permanent` mirrors `PermanentError`
/// (no retry); everything else is retried by [`queue::fail`].
#[derive(Debug)]
pub enum ProcessError {
    Permanent(String),
    Ask(AskError),
    Jev(jev::JevError),
    Store(StoreError),
    Postgres(postgres::Error),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Python stores f"{type(e).__name__}: {e}" in the job
            ProcessError::Permanent(m) => write!(f, "PermanentError: {m}"),
            ProcessError::Ask(AskError::Permanent(m)) => write!(f, "PermanentError: {m}"),
            ProcessError::Ask(AskError::Api(m)) => write!(f, "{m}"),
            ProcessError::Jev(e) => write!(f, "JevError: {e}"),
            ProcessError::Store(e) => write!(f, "StoreError: {e}"),
            ProcessError::Postgres(e) => write!(f, "PostgresError: {e}"),
        }
    }
}
impl std::error::Error for ProcessError {}

impl From<AskError> for ProcessError {
    fn from(e: AskError) -> Self {
        ProcessError::Ask(e)
    }
}
impl From<jev::JevError> for ProcessError {
    fn from(e: jev::JevError) -> Self {
        ProcessError::Jev(e)
    }
}
impl From<StoreError> for ProcessError {
    fn from(e: StoreError) -> Self {
        ProcessError::Store(e)
    }
}
impl From<crate::storage::S3Error> for ProcessError {
    fn from(e: crate::storage::S3Error) -> Self {
        ProcessError::Store(StoreError::S3(e))
    }
}
impl From<postgres::Error> for ProcessError {
    fn from(e: postgres::Error) -> Self {
        ProcessError::Postgres(e)
    }
}

/// Everything one job needs, from the `_context` query.
#[allow(dead_code)]
struct Context {
    message_id: i64,
    account: String,
    meta: MessageMeta,
    profile_id: i64,
    profile: Value,
    patterns: Vec<jev::Pattern>,
    s3_key: Option<String>,
}

pub struct Classifier<'a, C: S3Client + Sync, F> {
    pub settings: ClassifierSettings,
    pub pool: &'a Pool,
    pub store: &'a Store<C>,
    /// one fresh asker per worker thread (`asker_factory` in Python)
    pub asker_factory: F,
    pub stop: StopEvent,
    pub idle_wait: f64,
    pub reclaim_every: f64,
}

impl<C: S3Client + Sync, F> Classifier<'_, C, F>
where
    F: Fn() -> Box<dyn Asker> + Send + Sync,
{
    fn context(&self, message_id: i64) -> Result<Option<Context>, postgres::Error> {
        let mut conn = self.pool.get()?;
        let row = conn
            .query_opt(
                "SELECT m.id, a.address, m.rfc822_message_id, m.from_email, m.from_name, m.subject, m.sent_at,
                        m.in_reply_to, m.message_references, m.has_list_headers, p.id, p.profile,
                        (SELECT l.s3_key FROM message_locations l WHERE l.message_id = m.id
                         ORDER BY l.ingested_at DESC, l.id DESC LIMIT 1)
                 FROM messages m
                 JOIN accounts a ON a.id = m.account_id
                 JOIN profiles p ON p.id = a.profile_id
                 WHERE m.id = $1",
                &[&message_id],
            )?;
        let Some(row) = row else {
            return Ok(None);
        };
        let profile_id: i64 = row.get(10);
        let pats = profiles::patterns(&mut *conn, profile_id)?;
        Ok(Some(Context {
            message_id: row.get(0),
            account: row.get(1),
            meta: MessageMeta {
                rfc822_message_id: row.get(2),
                from_email: row.get(3),
                from_name: row.get(4),
                subject: row.get(5),
                sent_at: row.get::<_, Option<crate::pgtime::Ts>>(6).map(|t| t.0),
                in_reply_to: row.get(7),
                references: row.get(8),
                has_list_headers: row.get(9),
            },
            profile_id,
            profile: row.get(11),
            patterns: pats,
            s3_key: row.get(12),
        }))
    }

    fn exists(&self, message_id: i64, key: &str) -> Result<bool, postgres::Error> {
        let mut conn = self.pool.get()?;
        Ok(conn
            .query_opt(
                "SELECT 1 FROM classifications WHERE message_id = $1 AND classifier_key = $2",
                &[&message_id, &key],
            )?
            .is_some())
    }

    /// Returns "classified" or "skipped". Raises on failure.
    pub fn process(&self, job: &Job, asker: &mut dyn Asker) -> Result<&'static str, ProcessError> {
        let ctx = self
            .context(job.message_id)?
            .ok_or_else(|| ProcessError::Permanent("message row is gone".to_string()))?;
        let questions = jev::build_questions(&ctx.patterns);
        let key = jev::classifier_key(&questions, &self.settings.model, &ctx.profile);
        if self.exists(job.message_id, &key)? {
            let mut conn = self.pool.get()?;
            queue::complete(&mut *conn, job.id)?;
            return Ok("skipped");
        }
        let Some(s3_key) = ctx.s3_key.filter(|k| !k.is_empty()) else {
            return Err(ProcessError::Permanent("message has no stored location".to_string()));
        };
        let Some((raw, _)) = self.store.get_raw(&s3_key)? else {
            return Err(ProcessError::Permanent(format!(
                "raw message missing from S3: {s3_key}"
            )));
        };

        let state = jev::build_state(
            &ctx.profile,
            &ctx.account,
            &raw,
            &ctx.meta,
            self.settings.body_chars,
        );
        let result = asker.ask(&state, &questions)?;
        let d = jev::decide(&result.answers, &ctx.patterns, &self.settings.thresholds)?;

        let mut conn = self.pool.get()?;
        let mut tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers, usage,
                importance, action_bucket, tags, insight, source, needs_review, priority, signals)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (message_id, classifier_key) DO NOTHING",
            &[
                &job.message_id,
                &ctx.profile_id,
                &key,
                &result.model,
                &Value::Object(result.answers.clone()),
                &result.usage,
                &d.importance,
                &d.action_bucket,
                &d.tags,
                &d.insight,
                &d.source,
                &d.needs_review,
                // priority column is REAL; the crate wants an exact type match
                &(d.priority as f32),
                &Value::Object(d.signals.clone()),
            ],
        )?;
        queue::complete(&mut tx, job.id)?;
        tx.commit()?;
        Ok("classified")
    }

    fn run_job(&self, job: Job, asker: &mut dyn Asker) {
        if let Err(e) = self.process(&job, asker) {
            let permanent = matches!(e, ProcessError::Permanent(_));
            let status = {
                let mut conn = self.pool.get().expect("pool connection");
                queue::fail(
                    &mut *conn,
                    &job,
                    &e.to_string(),
                    self.settings.max_attempts,
                    self.settings.retry_delay,
                    3600.0,
                    permanent,
                )
                .expect("record failure")
            };
            let _ = status; // logged in Python; the port is silent like the other modules
        }
    }

    fn claim(&self) -> Result<Option<Job>, postgres::Error> {
        let mut conn = self.pool.get()?;
        queue::claim(&mut *conn, STAGE)
    }

    fn worker_loop(&self, keep_going: bool, done: &AtomicUsize) {
        let mut asker = (self.asker_factory)();
        while !self.stop.is_set() {
            let job = match self.claim() {
                Ok(job) => job,
                Err(_) => {
                    // "cannot claim job" — give up when draining
                    if !keep_going {
                        return;
                    }
                    None
                }
            };
            match job {
                None => {
                    if !keep_going {
                        return;
                    }
                    self.stop.wait_timeout(Duration::from_secs_f64(self.idle_wait));
                }
                Some(job) => {
                    self.run_job(job, &mut *asker);
                    done.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// Put back jobs left `running` by a worker that died. Returns how many.
    pub fn reclaim(&self) -> Result<u64, postgres::Error> {
        let mut conn = self.pool.get()?;
        queue::reclaim_stuck(
            &mut *conn,
            STAGE,
            self.settings.job_timeout,
            self.settings.max_attempts,
        )
    }

    fn run(&self, keep_going: bool) -> usize {
        let _ = self.reclaim();
        let done = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..self.settings.concurrency {
                scope.spawn(|| self.worker_loop(keep_going, &done));
            }
            if keep_going {
                while !self
                    .stop
                    .wait_timeout(Duration::from_secs_f64(self.reclaim_every))
                {
                    // "cannot reclaim stuck jobs" — the next pass retries
                    let _ = self.reclaim();
                }
            }
        });
        done.load(Ordering::SeqCst)
    }

    /// Process jobs until none are due, then return how many were processed.
    pub fn drain(&self) -> usize {
        self.run(false)
    }

    pub fn run_forever(&self) {
        self.run(true);
    }
}
