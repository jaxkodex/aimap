# aimap

Watches one or more IMAP mailboxes, copies every new message byte for byte
into an S3 bucket, and classifies it with [TypeSafe Jev](https://typesafe.ai):
importance, a suggested action and tags. Message metadata and labels go to
Postgres. Bodies stay in S3 only.

It is one Docker image run as three processes:

- `aimap run` polls IMAP, writes `.eml` files to S3, records their headers in
  Postgres and queues a `classify` job for each new message.
- `aimap classify` takes those jobs, reads the message from S3, asks Jev and
  stores the labels.
- `aimap api` serves the labelled mail over HTTP to the app, for users signed
  in with Firebase Authentication.

They share nothing but the bucket and the database, so a Jev outage never
stops ingestion, and each process scales and restarts on its own.

Accounts live in an encrypted file in the same bucket. You add or change
them with `aimap accounts ...`, and the running worker picks the change up on
its next poll.

## How it works

### Ingestion

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

### Classification

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

### Bucket layout

```
<S3_PREFIX>/
  config/accounts.json                              accounts, passwords encrypted
  raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml   Content-Type message/rfc822
                                                    metadata: imap-flags, imap-internaldate
  state/<account>/<mailbox>.json                    legacy checkpoints, read once on upgrade
```

Mailbox names are URL-encoded, so `[Gmail]/All Mail` becomes `%5BGmail%5D%2FAll%20Mail`.

### Database

| Table | One row per |
|---|---|
| `profiles` | Recipient context Jev classifies against. `default` exists from the start. |
| `accounts` | Address from the accounts file, with its profile. No credentials. |
| `mailbox_state` | Account and mailbox: `uidvalidity`, `last_uid`. |
| `messages` | Account and RFC 822 Message-ID: sender, subject, date, reply and list headers. A message without a Message-ID uses `sha256:<hash of the raw bytes>`. |
| `message_locations` | Mailbox, UIDVALIDITY and UID where a message is stored, with its S3 key and IMAP flags. The same message in `INBOX` and `[Gmail]/All Mail` has two locations and one `messages` row, so it is classified once. |
| `jobs` | Message and stage. The only stage today is `classify`. |
| `patterns` | Known pattern of a profile: insight, importance, action bucket, tags, example senders and subjects. |
| `classifications` | Message and classifier key: labels, `needs_review`, `priority`, signals, and the raw Jev answers. |

The `message_labels` view joins each message with its latest classification.

`aimap migrate` applies the numbered SQL files in
[`src/aimap/migrations`](src/aimap/migrations) that are missing, each in its
own transaction, under an advisory lock. `run` and `classify` exit at startup
if a migration is missing. To change the schema, add the next numbered file.
Never edit one that has shipped.

### API

`aimap api` listens on `$PORT` (default 8080). It only reads: nothing in the
mailbox, the bucket or the labels changes through it.

| Route | Returns |
|---|---|
| `GET /healthz` | `{"ok": true}` when Postgres answers. No token needed. |
| `GET /me` | The signed-in user's Firebase uid and email. |
| `GET /accounts` | Each account with its profile, message count and unread count. |
| `GET /home` | The Home screen: a brief, `act_now`, `waiting` and `sorted` sections. |
| `GET /messages` | Messages newest first, with their latest labels. |
| `GET /messages/{id}` | One message: metadata, labels, signals, reasons and mailboxes. |
| `GET /messages/{id}/body` | The text body, read from S3 for that request and not kept. |

`/home` takes `account`, `days` (the window, default 7) and `new_since` (for
the brief's `new` count, default 24 hours ago). `act_now` holds the `act_now`
and `verify` buckets, highest `priority` first. `waiting` holds `reply`, the
longest waiting first. Everything else is grouped under its pattern's insight,
its first tag or its bucket. Reasons ("Mentions a deadline") and group
summaries ("Uber, AWS Billing + 1 more") are templates over stored signals and
senders, so the API never calls Jev. Mail the account sent itself is left out.

`/messages` takes `account`, `filter` (`all`, `unread`, `flagged`,
`needs_review`), `limit` (up to 200) and `cursor`. Pass the response's
`next_cursor` to get the next page. It is `null` on the last one.

Unread and flagged come from the IMAP flags seen at ingestion. Mail read in
another client still shows as unread. `waiting` means Jev thinks someone
expects an answer, not that no reply was sent: Sent mail isn't matched yet.

#### Sign-in

The app signs in with the [Firebase Authentication](https://firebase.google.com/docs/auth)
SDK (Google, Apple or any provider you enable) and sends the ID token on every
request as `Authorization: Bearer <token>`. The API checks its signature
against Google's certificates, which it caches for as long as their headers
allow, and checks that its audience and issuer are `FIREBASE_PROJECT_ID`.
Tokens last an hour and the SDK renews them, so the API keeps no sessions.

Any Google account can sign in to a Firebase project, so the API also requires
a verified email listed in `AIMAP_ALLOWED_EMAILS`. A valid token for another
email gets `403`. A missing, expired or forged one gets `401`. With an empty
list nobody gets in, and `aimap api` refuses to start.

## Profiles and patterns

A profile is a JSON object that Jev reads as `recipient_profile`. Jev keeps no
memory between requests, so this is how it learns what matters to you. Any
shape works. This one does well:

```json
{
  "who": "Alex, a freelance designer in Lisbon.",
  "active_priorities": ["Getting paid by clients", "Renewing the studio lease"],
  "low_value": ["Retail coupons", "Social media digests"]
}
```

Patterns are the kinds of mail you already know how to handle. When Jev
matches one with enough confidence, its labels are used as they are, which is
more consistent than judging each message from scratch. A CSV has one row per
pattern, or one row per example if you repeat the insight:

```csv
insight,importance,action_bucket,tags,example_from,example_subject
Client invoices,High,act_now,work;billing,Client <billing@client.example>,Invoice 42
Shop promotions,Low,discard,shopping;promo,Shop <deals@shop.example>,20% off this weekend
```

`importance` is `Low`, `Medium` or `High`. `action_bucket` is one of the seven
buckets above. JSON files take a list of objects with the same fields, plus
`examples` as a list of `{"from", "subject"}`.

```sh
aimap profiles set default --file profile.json   # create or replace
aimap patterns import default patterns.csv       # replaces the profile's patterns
aimap profiles list
aimap patterns list default
```

Every account uses `default` until you move it. For separate work and
personal labels:

```sh
aimap profiles set work --file work.json
aimap patterns import work work-patterns.csv
aimap accounts set-profile me@company.example work
```

## Accounts

### The accounts file

`config/accounts.json` holds one record per account. Host, port, mailboxes
and the enabled flag are plain JSON, so you can read the file. The password
is a [Fernet](https://cryptography.io/en/latest/fernet/) token, which is
AES-128-CBC with an HMAC-SHA256 tag. It's encrypted with `AIMAP_SECRET_KEY`,
which lives in the service's environment and never in the bucket. Anyone with
read access to the bucket but not the key sees ciphertext only.

Each token encrypts the account's user together with its password. Decryption
checks the user, so a token copied onto another account's record fails.

Writes are conditional on the file's ETag (`If-Match` / `If-None-Match`). If
two people run `aimap accounts` at once, the second write fails, rereads
the file and retries instead of silently overwriting the first change.

### Commands

```sh
aimap keygen                                   # print a new AIMAP_SECRET_KEY

aimap accounts add me@gmail.com --host imap.gmail.com [--port 993] [--mailbox INBOX --mailbox Sent]
aimap accounts list                            # never prints passwords; KEY column shows if it decrypts
aimap accounts update me@gmail.com --mailbox INBOX --mailbox Archive
aimap accounts set-password me@gmail.com
aimap accounts disable me@gmail.com            # stop syncing, keep the record
aimap accounts enable me@gmail.com
aimap accounts remove me@gmail.com             # stored mail stays in the bucket, its rows in Postgres
aimap accounts set-profile me@gmail.com work   # stored in Postgres, not in the file
aimap accounts rotate-key
```

`add` and `set-password` prompt for the password with no echo. In scripts,
pipe it with `--password-stdin`. Neither command accepts the password as an
argument, so it never lands in shell history or `ps`. Before saving, they log
in to the IMAP server and open every mailbox, and they refuse to save if that
fails. `--no-verify` skips the check.

For Gmail, use an [app password](https://myaccount.google.com/apppasswords),
which needs 2-Step Verification. Spaces in pasted app passwords are removed.

### Rotating the key

1. Generate a key and put it first: `AIMAP_SECRET_KEY=<new>,<old>`. Every
   listed key decrypts. The first one encrypts.
2. Run `aimap accounts rotate-key` to re-encrypt every password with `<new>`.
3. Remove `<old>` from `AIMAP_SECRET_KEY` and redeploy.

If the key is lost, the passwords are gone. Set a new key and run
`set-password` for each account.

### Env-only mode

For a single fixed mailbox with no accounts file, set `IMAP_USER`,
`IMAP_PASSWORD` and `IMAP_HOST`. For more accounts, add `IMAP_USER2`,
`IMAP_PASSWORD2` and so on. When `IMAP_USER` is set, `ACCOUNTS_SOURCE`
defaults to `env`. Changes need a restart, and `AIMAP_SECRET_KEY` isn't used.

## Configuration

Everything comes from environment variables. For local runs, the worker also
reads a `.env` file in the working directory. Copy `.env.example` to start.

| Variable | Default | Notes |
|---|---|---|
| `S3_BUCKET` | required | |
| `S3_PREFIX` | empty | Key prefix inside the bucket. |
| `S3_ENDPOINT_URL` | AWS | Set for Railway buckets, Cloudflare R2, SeaweedFS and other S3-compatible stores. |
| `S3_REGION` | from `AWS_REGION` | |
| `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | AWS default chain | If unset, boto3 reads `AWS_ACCESS_KEY_ID` and the rest of its usual sources. |
| `S3_FORCE_PATH_STYLE` | `false` | Set `true` for stores without virtual-host buckets, such as SeaweedFS. |
| `DATABASE_URL` | required | Postgres connection string, e.g. `postgresql://user:pass@host:5432/aimap`. |
| `AIMAP_SECRET_KEY` | required for `bucket` | Comma-separated Fernet keys, first one encrypts. `aimap keygen` makes one. Not needed by `classify`, `migrate`, `backfill`, `jobs`, `profiles` or `patterns`. |
| `ACCOUNTS_SOURCE` | `bucket`, or `env` if `IMAP_USER` is set | |
| `ACCOUNTS_PATH` | `config/accounts.json` | Relative to `S3_PREFIX`. |
| `IMAP_USER`, `IMAP_PASSWORD`, `IMAP_HOST`, `IMAP_PORT`, `IMAP_MAILBOX` | | Env-only mode, see above. |
| `POLL_INTERVAL_SECONDS` | `60` | Also how long an account change takes to apply. |
| `INITIAL_FETCH_COUNT` | `100` | Messages to take on a mailbox's first sync. `0` means all. |
| `BATCH_SIZE` | `25` | Messages per checkpoint write. |
| `IMAP_TIMEOUT_SECONDS` | `60` | Socket timeout. |
| `MAX_BACKOFF_SECONDS` | `1800` | Upper bound on the retry delay for a failing account. |
| `JEV_API_KEY` | required for `classify` | TypeSafe API key. `TYPESAFE_API_KEY` also works. |
| `JEV_MODEL` | `jev-1.13.0` | Pinned so the thresholds below keep meaning the same thing. |
| `JEV_PATTERN_CONFIDENCE` | `0.5` | Lowest pattern confidence at which its labels are copied. |
| `JEV_TAG_THRESHOLD` | `0.8` | Probability at which a tag applies when no pattern matched. |
| `JEV_REVIEW_CONFIDENCE` | `0.4` | Importance or action confidence below which `needs_review` is set. |
| `JEV_BODY_CHARS` | `1500` | Body text sent to Jev, cut at a word boundary. |
| `JEV_TIMEOUT_SECONDS` | `120` | Per request. |
| `CLASSIFY_CONCURRENCY` | `4` | Jev requests in flight per `classify` process. |
| `CLASSIFY_MAX_ATTEMPTS` | `5` | Attempts before a job is marked `failed`. |
| `CLASSIFY_RETRY_SECONDS` | `30` | First retry delay, doubled per attempt, capped at one hour. |
| `CLASSIFY_JOB_TIMEOUT_SECONDS` | `600` | A job `running` longer than this goes back to the queue. |
| `FIREBASE_PROJECT_ID` | required for `api` | The Firebase project the app signs in to. |
| `AIMAP_ALLOWED_EMAILS` | required for `api` | Comma-separated emails that may use the API. |
| `PORT` | `8080` | Port `api` listens on. Railway sets it. |
| `API_HOST` | `0.0.0.0` | |
| `API_POOL_SIZE` | `10` | Postgres connections for `api`. |
| `LOG_LEVEL` | `INFO` | |
| `LOG_FORMAT` | `json` | `text` for human-readable local logs. |

Logs never include passwords or message content. They do include account
addresses and UIDs.

## Commands

```
aimap run                  poll IMAP forever (container default)
aimap once                 one pass, exit 1 if any account failed
aimap check                verify S3 and Postgres, and log in to every enabled account

aimap classify             take classify jobs forever
aimap classify --once      process every due job, then exit
aimap jobs status          job counts per stage and status
aimap jobs retry [--done]  requeue failed jobs, and with --done finished ones too

aimap api                  serve the HTTP API on $PORT

aimap migrate              apply missing schema migrations
aimap backfill             record messages that are in S3 but not in Postgres, and queue them
```

`backfill` is for mail stored before the database existed, or for rebuilding
the database from the bucket. It skips keys already recorded, so running it
twice is safe. It does not touch checkpoints.

## Run locally

With [uv](https://docs.astral.sh/uv/):

```sh
uv sync
cp .env.example .env                           # S3, DATABASE_URL, JEV_API_KEY
echo "AIMAP_SECRET_KEY=$(uv run aimap keygen)" >> .env
uv run aimap migrate
uv run aimap accounts add me@gmail.com --host imap.gmail.com
uv run aimap run                               # in one terminal
uv run aimap classify                          # in another
uv run aimap api                               # and a third, with FIREBASE_PROJECT_ID and AIMAP_ALLOWED_EMAILS
```

Without AWS, Docker Compose starts Postgres and [SeaweedFS](https://github.com/seaweedfs/seaweedfs)
as a local S3 store, creates the bucket, applies the migrations and runs all
three processes, with the API on http://localhost:8080:

```sh
echo "AIMAP_SECRET_KEY=$(docker run --rm $(docker build -q .) keygen)" > .env
echo "JEV_API_KEY=..." >> .env
echo "FIREBASE_PROJECT_ID=..." >> .env
echo "AIMAP_ALLOWED_EMAILS=me@gmail.com" >> .env
docker compose up --build -d
docker compose run --rm -it worker accounts add me@gmail.com --host imap.gmail.com
docker compose run --rm -T worker profiles set default --file - < profile.json
docker compose logs -f worker classifier       # picks the account up within a minute
```

## Deploy on Railway

1. Create a project and add a service from this GitHub repo. Railway reads
   `railway.toml` and builds the `Dockerfile`. This is the ingest worker.
2. Add a bucket to the project, or use any S3-compatible bucket you already
   have. Add a Postgres database.
3. On the service, set `S3_BUCKET`, `S3_ENDPOINT_URL`, `S3_REGION`,
   `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`. If you use a Railway bucket,
   reference its variables instead of pasting values, e.g.
   `S3_BUCKET=${{Bucket.BUCKET}}`. The names on the right come from the
   bucket's Variables tab. Set `DATABASE_URL=${{Postgres.DATABASE_URL}}`.
4. Set `AIMAP_SECRET_KEY` to the output of `aimap keygen`. Also store it in
   your password manager.
5. Deploy. `railway.toml` runs `aimap migrate` before each deploy starts. The
   service needs no public domain or volume. The logs show
   `no accounts configured; waiting` until you add one.
6. Add a second service from the same repo for the classifier. Give it the
   same S3 and `DATABASE_URL` variables, plus `JEV_API_KEY`, and set its
   start command to `aimap classify`. It does not need `AIMAP_SECRET_KEY`.
   To classify faster, raise `CLASSIFY_CONCURRENCY` or add replicas. Jobs are
   claimed with row locks, so replicas never share one.
7. Add a third service from the same repo for the API, with start command
   `aimap api`. Give it the same S3 and `DATABASE_URL` variables, plus
   `FIREBASE_PROJECT_ID` and `AIMAP_ALLOWED_EMAILS`. It needs neither
   `AIMAP_SECRET_KEY` nor `JEV_API_KEY`. Generate a public domain for it and
   set its healthcheck path to `/healthz`. Railway sets `PORT`.
8. Add accounts from your machine with the service's variables injected. The
   bucket endpoint has to be reachable from where you run this:

   ```sh
   railway link                                # pick the project and service
   railway run uv run aimap accounts add me@gmail.com --host imap.gmail.com
   ```

   Or open a shell in the running container with `railway ssh` and run
   `aimap accounts add ...` there.

The worker logs `account added` and `first sync` within one poll interval,
with no redeploy. Set a profile with
`railway run uv run aimap profiles set default --file profile.json`. If the
bucket already holds mail from before Postgres, run `aimap backfill` once to
queue it.

## Development

```sh
uv sync
docker run -d --rm --name aimap-pg -p 55432:5432 -e POSTGRES_PASSWORD=pg postgres:16
TEST_DATABASE_URL=postgresql://postgres:pg@localhost:55432/postgres uv run pytest
uv run ruff check .
```

Tests use [moto](https://github.com/getmoto/moto) for S3, an in-memory fake
mailbox and a fake Jev client. They need no network access and no
credentials. Tests that need Postgres create a throwaway database next to
`TEST_DATABASE_URL`, apply the migrations and drop it at the end. Without
`TEST_DATABASE_URL` they are skipped, except in CI, where they fail.

## Roadmap

- [x] IMAP to S3 ingestion loop with checkpoints
- [x] Encrypted, live-reloaded accounts file
- [ ] OAuth2 (XOAUTH2) refresh tokens as an alternative to app passwords
- [x] Message metadata, checkpoints and a job queue in Postgres
- [x] Classify each message with [TypeSafe Jev](https://typesafe.ai): importance,
      action bucket, tags, priority
- [x] Profiles and pattern catalogues in Postgres, one profile per account
- [x] Read-only HTTP API for the app, behind Firebase Authentication
- [ ] Embeddings: an `embed` job stage that reads the `.eml` from S3 and writes
      to a pgvector table (Railway needs its pgvector Postgres image for this)
- [ ] Re-run `decide` over stored Jev answers after a threshold change, with
      no new Jev calls
- [ ] Feedback loop: turn corrected labels into patterns

## License

[MIT](LICENSE)
