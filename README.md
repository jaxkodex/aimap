# aimap

A worker that watches one or more IMAP mailboxes and copies every new message,
byte for byte, into an S3 bucket. It runs as a single container and polls on
an interval, so it deploys on Railway (or anywhere Docker runs) without a
database or a volume.

Accounts live in an encrypted file in the same bucket. You add or change
them with `aimap accounts ...`, and the running worker picks the change up on
its next poll.

Classification of the stored messages is the next stage. See [Roadmap](#roadmap).

## How it works

Every `POLL_INTERVAL_SECONDS` the worker:

1. Reloads the account list. For the accounts file, a `HEAD` request checks
   whether its ETag changed, and the worker downloads the file only when it has.
2. For each enabled account and mailbox:
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
retrying at full speed. Changing the account, for example with
`aimap accounts set-password`, clears the backoff so the fix is tried on the
next poll. `SIGTERM` stops the worker between batches, and the next start
resumes from the last checkpoint.

### Bucket layout

```
<S3_PREFIX>/
  config/accounts.json                              accounts, passwords encrypted
  raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml   Content-Type message/rfc822
                                                    metadata: imap-flags, imap-internaldate
  state/<account>/<mailbox>.json                    {"uidvalidity", "last_uid", "updated_at"}
```

Mailbox names are URL-encoded, so `[Gmail]/All Mail` becomes `%5BGmail%5D%2FAll%20Mail`.

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
aimap accounts remove me@gmail.com             # stored mail and checkpoints stay in the bucket
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
| `AIMAP_SECRET_KEY` | required for `bucket` | Comma-separated Fernet keys, first one encrypts. `aimap keygen` makes one. |
| `ACCOUNTS_SOURCE` | `bucket`, or `env` if `IMAP_USER` is set | |
| `ACCOUNTS_PATH` | `config/accounts.json` | Relative to `S3_PREFIX`. |
| `IMAP_USER`, `IMAP_PASSWORD`, `IMAP_HOST`, `IMAP_PORT`, `IMAP_MAILBOX` | | Env-only mode, see above. |
| `POLL_INTERVAL_SECONDS` | `60` | Also how long an account change takes to apply. |
| `INITIAL_FETCH_COUNT` | `100` | Messages to take on a mailbox's first sync. `0` means all. |
| `BATCH_SIZE` | `25` | Messages per checkpoint write. |
| `IMAP_TIMEOUT_SECONDS` | `60` | Socket timeout. |
| `MAX_BACKOFF_SECONDS` | `1800` | Upper bound on the retry delay for a failing account. |
| `LOG_LEVEL` | `INFO` | |
| `LOG_FORMAT` | `json` | `text` for human-readable local logs. |

Logs never include passwords or message content. They do include account
addresses and UIDs.

## Worker commands

```
aimap run      poll forever (container default)
aimap once     one pass, exit 1 if any account failed
aimap check    verify S3 access and log in to every enabled account
```

## Run locally

With [uv](https://docs.astral.sh/uv/):

```sh
uv sync
cp .env.example .env                           # S3 settings
echo "AIMAP_SECRET_KEY=$(uv run aimap keygen)" >> .env
uv run aimap accounts add me@gmail.com --host imap.gmail.com
uv run aimap run
```

Without AWS, Docker Compose starts [SeaweedFS](https://github.com/seaweedfs/seaweedfs)
as a local S3 store, creates the bucket and runs the worker against it:

```sh
echo "AIMAP_SECRET_KEY=$(docker run --rm $(docker build -q .) keygen)" > .env
docker compose up --build -d
docker compose run --rm -it worker accounts add me@gmail.com --host imap.gmail.com
docker compose logs -f worker                  # picks the account up within a minute
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
4. Set `AIMAP_SECRET_KEY` to the output of `aimap keygen`. Also store it in
   your password manager.
5. Deploy. The service needs no public domain or volume. The logs show
   `no accounts configured; waiting` until you add one.
6. Add accounts from your machine with the service's variables injected. The
   bucket endpoint has to be reachable from where you run this:

   ```sh
   railway link                                # pick the project and service
   railway run uv run aimap accounts add me@gmail.com --host imap.gmail.com
   ```

   Or open a shell in the running container with `railway ssh` and run
   `aimap accounts add ...` there.

The worker logs `account added` and `first sync` within one poll interval,
with no redeploy.

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
- [x] Encrypted, live-reloaded accounts file
- [ ] OAuth2 (XOAUTH2) refresh tokens as an alternative to app passwords
- [ ] Parse stored messages into JSON (headers, text body, attachment list)
- [ ] Classify each message with [TypeSafe Jev](https://typesafe.ai): importance,
      suggested action, tags. Results go to `classified/` in the same bucket.
- [ ] Per-recipient profile and pattern catalogue loaded from the bucket, not
      from code

## License

[MIT](LICENSE)
