# Configuration

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
| `IMAP_USER`, `IMAP_PASSWORD`, `IMAP_HOST`, `IMAP_PORT`, `IMAP_MAILBOX` | | [Env-only mode](accounts.md#env-only-mode). |
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
| `AIMAP_DRAFT_BASE_URL` | `https://api.deepseek.com/v1` | Base URL for draft generation. `/chat/completions` is appended. |
| `AIMAP_DRAFT_API_KEY` | unset | API key for draft generation. Unset turns the feature off. |
| `AIMAP_DRAFT_MODEL` | `deepseek-chat` | Model name for draft generation. |
| `AIMAP_DRAFT_TIMEOUT` | `45` | Seconds for the draft HTTP call. |
| `AIMAP_DRAFT_MAX_TOKENS` | `700` | Response cap for draft generation. |
| `LOG_LEVEL` | `INFO` | |
| `LOG_FORMAT` | `json` | `text` for human-readable local logs. |

Logs never include passwords or message content. They do include account
addresses and UIDs.
