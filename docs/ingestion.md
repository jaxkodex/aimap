# Ingestion

Every `POLL_INTERVAL_SECONDS` the worker:

1. Reloads the account list. For the accounts file, a `HEAD` request checks
   whether its ETag changed, and the worker downloads the file only when it has.
2. For each enabled account and mailbox:
   1. Opens the mailbox read-only and reads its `UIDVALIDITY`.
   2. Loads the mailbox checkpoint from the `mailbox_state` table.
   3. Fetches messages with a UID above the checkpoint using `BODY.PEEK[]`,
      so nothing gets marked as read.
   4. Writes each one to `raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml`.
   5. After each batch of `BATCH_SIZE` messages, commits one Postgres
      transaction that records the headers, queues a `classify` job for each
      message it had not seen, and advances the checkpoint.

S3 is written first, Postgres second. A crash before the commit re-uploads the
same keys on the next pass, and the database upserts do nothing the second
time. If the server changes `UIDVALIDITY`, the worker starts that mailbox over
under the new value and leaves the old objects alone. The messages are already
in Postgres by Message-ID, so they are not classified again.

Versions before Postgres kept checkpoints in `state/<account>/<mailbox>.json`.
The first sync of a mailbox with no database checkpoint imports that file, so
an upgrade does not refetch mail. The file is no longer written.

On the first sync of a mailbox it takes the newest `INITIAL_FETCH_COUNT`
messages (default 100, `0` for the whole mailbox) and follows new mail from
there.

A failing account backs off exponentially, from `POLL_INTERVAL_SECONDS` up to
`MAX_BACKOFF_SECONDS`, while the other accounts keep syncing. Repeated bad
logins can get an account locked, which is why it backs off instead of
retrying at full speed. Changing the account, for example with
`aimap accounts set-password`, clears the backoff so the fix is tried on the
next poll. `SIGTERM` stops the worker between batches, and the next start
resumes from the last checkpoint.

Where the files and rows end up is in [Storage](storage.md). How to add and change accounts is in [Accounts](accounts.md).
