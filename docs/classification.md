# Classification

`aimap classify` runs `CLASSIFY_CONCURRENCY` threads. Each one claims a
pending job with `SELECT ... FOR UPDATE SKIP LOCKED`, so no two threads or
replicas get the same message, and then:

1. Loads the message headers, the profile of its account and the profile's
   patterns.
2. Builds the questions and a classifier key, a hash of the questions, the
   model and the profile. If the message already has a classification with
   that key, the job is marked done without calling Jev.
3. Reads the `.eml` from S3 and builds the Jev state: sender, subject, facts
   computed from headers (bulk mail, reply in a thread, sent by the account
   itself, attachment names) and the text body cut to `JEV_BODY_CHARS`. The
   body only exists in memory.
4. Sends one Jev request with every question in it:
   - `pattern`, a choice over the profile's known patterns plus
     `none_of_these`.
   - `importance`, a score from Low to High against the profile.
   - `action`, a choice over seven buckets: `discard`, `batch_review`,
     `skim`, `review`, `verify`, `act_now`, `reply`.
   - `tag:<name>`, one yes/no question per tag used by the patterns.
   - `sig:<name>`, generic yes/no signals (written by a person, asks to act,
     time-sensitive, security event, about a priority, promotional) that add
     up to a `priority` number.
5. If Jev picks a pattern with confidence of at least
   `JEV_PATTERN_CONFIDENCE`, the pattern's labels are copied. Otherwise the
   independent answers are used and `needs_review` is set. It is also set when
   the two routes disagree or a confidence is below `JEV_REVIEW_CONFIDENCE`.
6. Inserts the classification, with the raw Jev answers, and marks the job
   done in one transaction.

A failed job goes back to the queue after `CLASSIFY_RETRY_SECONDS`, doubling
on each attempt, and is marked `failed` after `CLASSIFY_MAX_ATTEMPTS`. A
message whose `.eml` is gone from S3 fails at once. A job left `running` for
longer than `CLASSIFY_JOB_TIMEOUT_SECONDS`, for example because the process was
killed, goes back to the queue. `aimap jobs status` shows the counts and
`aimap jobs retry` requeues the failed ones.

Changing a profile, its patterns or `JEV_MODEL` changes the classifier key.
New mail uses it right away. To classify stored mail again, run
`aimap jobs retry --done`. Messages whose key did not change are skipped
without a Jev call. Every run adds a row to `classifications`, so the old
labels stay for comparison.

What Jev classifies against, and how to teach it your mail, is in [Profiles and patterns](profiles.md).
