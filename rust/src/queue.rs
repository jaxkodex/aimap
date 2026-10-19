//! Port of `src/aimap/queue.py`: the jobs table as a work queue.
//!
//! A worker claims one pending job with `FOR UPDATE SKIP LOCKED`, so two
//! workers never get the same row, and marks it `running`. When the work is
//! done it marks it `done`. On error it goes back to `pending` with an
//! exponential delay, or to `failed` after `max_attempts`. A job left
//! `running` by a worker that died is put back by `reclaim_stuck`.

use postgres::GenericClient;


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    pub message_id: i64,
    pub stage: String,
    /// including the current one
    pub attempts: i32,
}

pub fn claim<C: GenericClient>(conn: &mut C, stage: &str) -> Result<Option<Job>, postgres::Error> {
    let row = conn.query_opt(
        "UPDATE jobs SET status = 'running', locked_at = now(), attempts = attempts + 1, updated_at = now()
         WHERE id = (
             SELECT id FROM jobs
             WHERE stage = $1 AND status = 'pending' AND run_after <= now()
             ORDER BY run_after, id
             FOR UPDATE SKIP LOCKED
             LIMIT 1)
         RETURNING id, message_id, stage, attempts",
        &[&stage],
    )?;
    Ok(row.map(|row| Job {
        id: row.get(0),
        message_id: row.get(1),
        stage: row.get(2),
        attempts: row.get(3),
    }))
}

pub fn complete<C: GenericClient>(conn: &mut C, job_id: i64) -> Result<(), postgres::Error> {
    conn.execute(
        "UPDATE jobs SET status = 'done', locked_at = NULL, last_error = NULL, updated_at = now()
         WHERE id = $1",
        &[&job_id],
    )?;
    Ok(())
}

/// Record a failure. Returns the new status: 'pending' (will retry) or 'failed'.
pub fn fail<C: GenericClient>(
    conn: &mut C,
    job: &Job,
    error: &str,
    max_attempts: i32,
    base_delay: f64,
    max_delay: f64,
    permanent: bool,
) -> Result<&'static str, postgres::Error> {
    let (status, delay) = if permanent || job.attempts >= max_attempts {
        ("failed", 0.0)
    } else {
        ("pending", max_delay.min(base_delay * 2f64.powi(job.attempts - 1)))
    };
    let error = truncate(error, 2000);
    conn.execute(
        "UPDATE jobs SET status = $1, locked_at = NULL, last_error = $2, updated_at = now(),
                        run_after = now() + make_interval(secs => $3)
         WHERE id = $4",
        &[&status, &error, &delay, &job.id],
    )?;
    Ok(status)
}

/// Put back jobs that have been `running` longer than timeout seconds.
/// Returns how many.
pub fn reclaim_stuck<C: GenericClient>(
    conn: &mut C,
    stage: &str,
    timeout: f64,
    max_attempts: i32,
) -> Result<u64, postgres::Error> {
    conn.execute(
        "UPDATE jobs SET status = CASE WHEN attempts >= $1 THEN 'failed' ELSE 'pending' END,
                        locked_at = NULL, updated_at = now(),
                        last_error = coalesce(last_error, 'worker stopped while running the job')
         WHERE stage = $2 AND status = 'running' AND locked_at < now() - make_interval(secs => $3)",
        &[&max_attempts, &stage, &timeout],
    )
}

/// Send jobs in the given statuses back to pending with a fresh attempt count.
pub fn requeue<C: GenericClient>(conn: &mut C, stage: &str, statuses: &[&str]) -> Result<u64, postgres::Error> {
    conn.execute(
        "UPDATE jobs SET status = 'pending', attempts = 0, run_after = now(), locked_at = NULL,
                        last_error = NULL, updated_at = now()
         WHERE stage = $1 AND status = ANY($2)",
        &[&stage, &statuses],
    )
}

/// (stage, status, count) rows, ordered by stage, status.
pub fn counts<C: GenericClient>(conn: &mut C) -> Result<Vec<(String, String, i64)>, postgres::Error> {
    Ok(conn
        .query("SELECT stage, status, count(*) FROM jobs GROUP BY 1, 2 ORDER BY 1, 2", &[])?
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}
