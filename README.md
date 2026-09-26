# aimap

A worker that watches one or more IMAP mailboxes and copies every new message,
byte for byte, into an S3 bucket. It runs as a single container and polls on
an interval, so it deploys on Railway (or anywhere Docker runs) without a
database or a volume.

Classification of the stored messages is the next stage. See [Roadmap](#roadmap).

## How it works

Every `POLL_INTERVAL_SECONDS` the worker, for each account and mailbox:

1. Opens the mailbox read-only and reads its `UIDVALIDITY`.
2. Loads the checkpoint `state/<account>/<mailbox>.json` from the bucket.
3. Fetches messages with a UID above the checkpoint using `BODY.PEEK[]`,
   so nothing gets marked as read.
4. Writes each one to `raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml`.
5. Advances the checkpoint after each batch of `BATCH_SIZE` messages.

The checkpoint lives in the bucket, so the container is stateless. A crash
mid-batch re-uploads the same keys on the next pass, which is safe. If the
server changes `UIDVALIDITY`, the worker starts that mailbox over under the new
value and leaves the old objects alone.

On the first sync of a mailbox it takes the newest `INITIAL_FETCH_COUNT`
messages (default 100, `0` for the whole mailbox) and follows new mail from
there.

A failing account backs off exponentially, from `POLL_INTERVAL_SECONDS` up to
`MAX_BACKOFF_SECONDS`, while the other accounts keep syncing. Repeated bad
logins can get an account locked, which is why it backs off instead of
retrying at full speed. `SIGTERM` stops the worker between messages, and the
next start resumes from the last checkpoint.

### Bucket layout

```
<S3_PREFIX>/
  raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml   Content-Type message/rfc822
                                                    metadata: imap-flags, imap-internaldate
  state/<account>/<mailbox>.json                    {"uidvalidity", "last_uid", "updated_at"}
```

Mailbox names are URL-encoded, so `[Gmail]/All Mail` becomes `%5BGmail%5D%2FAll%20Mail`.

## Configuration

Everything comes from environment variables. For local runs, the worker also
reads a `.env` file in the working directory. Copy `.env.example` to start.

| Variable | Default | Notes |
|---|---|---|
| `IMAP_USER`, `IMAP_PASSWORD` | required | Use an app password for Gmail or Outlook. |
| `IMAP_HOST` | required | e.g. `imap.gmail.com` |
| `IMAP_PORT` | `993` | Implicit TLS only. |
| `IMAP_MAILBOX` | `INBOX` | Comma-separated for several, e.g. `INBOX,Archive`. |
| `IMAP_USER2`, `IMAP_PASSWORD2`, ... | | More accounts. `IMAP_HOST2`, `IMAP_PORT2` and `IMAP_MAILBOX2` fall back to the unsuffixed values. |
| `S3_BUCKET` | required | |
| `S3_PREFIX` | empty | Key prefix inside the bucket. |
| `S3_ENDPOINT_URL` | AWS | Set for Railway buckets, Cloudflare R2, SeaweedFS and other S3-compatible stores. |
| `S3_REGION` | from `AWS_REGION` | |
| `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | AWS default chain | If unset, boto3 reads `AWS_ACCESS_KEY_ID` and the rest of its usual sources. |
| `S3_FORCE_PATH_STYLE` | `false` | Set `true` for stores without virtual-host buckets, such as SeaweedFS. |
| `POLL_INTERVAL_SECONDS` | `60` | |
| `INITIAL_FETCH_COUNT` | `100` | Messages to take on a mailbox's first sync. `0` means all. |
| `BATCH_SIZE` | `25` | Messages per checkpoint write. |
| `IMAP_TIMEOUT_SECONDS` | `60` | Socket timeout. |
| `MAX_BACKOFF_SECONDS` | `1800` | Upper bound on the retry delay for a failing account. |
| `LOG_LEVEL` | `INFO` | |
| `LOG_FORMAT` | `json` | `text` for human-readable local logs. |

Logs never include passwords or message content. They do include account
addresses and UIDs.

## Commands

```
aimap run      poll forever (container default)
aimap once     one pass, exit 1 if any account failed
aimap check    verify S3 access and log in to every configured mailbox
```

## Run locally

With [uv](https://docs.astral.sh/uv/):

```sh
uv sync
cp .env.example .env         # fill in IMAP and S3 settings
uv run aimap check
uv run aimap run
```

Without AWS, Docker Compose starts [SeaweedFS](https://github.com/seaweedfs/seaweedfs)
as a local S3 store, creates the bucket and runs the worker against it:

```sh
cp .env.example .env         # fill in the IMAP settings only
docker compose up --build
# S3 API on http://localhost:8333, any access key works
```

## Deploy on Railway

1. Create a project and add a service from this GitHub repo. Railway reads
   `railway.toml` and builds the `Dockerfile`.
2. Add a bucket to the project, or use any S3-compatible bucket you already
   have.
3. On the service, set `S3_BUCKET`, `S3_ENDPOINT_URL`, `S3_REGION`,
   `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`. If you use a Railway bucket,
   reference its variables instead of pasting values, e.g.
   `S3_BUCKET=${{Bucket.BUCKET}}`. The names on the right come from the
   bucket's Variables tab.
4. Set the `IMAP_*` variables for each account.
5. Deploy. The service needs no public domain or volume. Check the deploy logs
   for `worker started` and `stored messages`.

To validate settings before the loop starts, run `aimap check` once as a
one-off command, or set it as the start command for a single deploy.

## Development

```sh
uv sync
uv run pytest
uv run ruff check .
```

Tests use [moto](https://github.com/getmoto/moto) for S3 and an in-memory
fake mailbox. They need no network access and no credentials.

## Roadmap

- [x] IMAP to S3 ingestion loop with checkpoints
- [ ] Parse stored messages into JSON (headers, text body, attachment list)
- [ ] Classify each message with [TypeSafe Jev](https://typesafe.ai): importance,
      suggested action, tags. Results go to `classified/` in the same bucket.
- [ ] Per-recipient profile and pattern catalogue loaded from the bucket, not
      from code
