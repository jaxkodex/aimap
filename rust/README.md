# Rust port

Leaf-first migration of `src/aimap` to Rust. Each module here mirrors a Python
module of the same name and is pinned to identical behavior by ported unit
tests plus golden fixtures (see below).

## Status

| Python module | Rust | Pinned by |
|---|---|---|
| `parse.py` | `src/parse.rs` | `tests/parse_unit.rs` + `tests/parse_golden.rs` + 908-message production diff |
| `crypto.py` | `src/crypto.rs` | `tests/crypto_unit.rs` + Python-generated Fernet vectors; bidirectionally verified with `cryptography` |
| `jev.py` | `src/jev.rs` | `tests/jev_unit.rs` + goldens (questions, classifier keys, decisions, states) |
| `draft.py` | `src/draft.rs` | `tests/draft_unit.rs` (pure) + `tests/inbox_draft_db.rs` (`generate_draft` end-to-end: real Postgres + S3 fake + model fake) |
| `db.py` | `src/db.rs` | `tests/queue_db.rs` against real Postgres; migration SQL embedded from `src/aimap/migrations` via `include_dir!`; `open_pool` ported as `db::Pool` (mutex + condvar, connections return on drop) |
| `queue.py` | `src/queue.rs` | `tests/queue_db.rs` on real Postgres, incl. the 4-thread claim race and SKIP LOCKED semantics |
| `storage.py` | `src/storage.rs` + `src/s3.rs` | `tests/storage_unit.rs` (in-memory fake) + `tests/s3_transport.rs`: SigV4 golden vs a Python-computed signature, live HTTP integration against moto |
| `catalog.py` | `src/catalog.rs` | `tests/catalog_db.rs` on real Postgres (idempotent batches, one-message-two-mailboxes, atomic rollback) |
| `home.py` | `src/home.rs` | `tests/home_unit.rs` — full port of `test_home.py` |
| `inbox.py` | `src/inbox.rs` | `tests/inbox_draft_db.rs` (needs_reply, cursors, paging, get_message/set_state/get_thread on real Postgres) |
| `config.py` | `src/config.rs` | `tests/config_unit.rs` (port of the config parts of `test_config.py`); full `S3Config` moved here and re-exported from `storage` |
| `classifier.py` | `src/classifier.rs` | `tests/classifier_db.rs` — full port of `test_classifier.py`: drain/run_forever with `concurrency` worker threads over `db::Pool`, skip-on-same-key, retry-then-succeed, permanent S3 miss, reclaim of dead workers' jobs, stop-event shutdown. The model call sits behind the `Asker` trait (fakes, like the Python suite); the real TypeSafe System One transport lives in `src/typesafe.rs` (wire-level `noul` mapping and response validation pinned by `tests/typesafe_unit.rs`, including a full HTTP exchange over an in-memory wire) |
| `accounts.py` | `src/accounts.rs` | `tests/accounts_unit.rs` — full port of `test_accounts.py` + the env-account parts of `test_config.py`: env source with fallbacks and numeric suffix sorting, bucket file parse/dump (JSON shape byte-compatible), ETag-conditional writes with retry, last-good-copy on broken files, undecryptable accounts skipped |
| `profiles.py` | `src/profiles.rs` | `tests/profiles_db.rs` on real Postgres (put/get/assign, patterns round-trip, validation, duplicates) + `parse_patterns` (JSON and a hand-rolled CSV reader pinned to Python's `csv` defaults) |
| `ingest.py` | `src/ingest.rs` | `tests/ingest_db.rs` — full port of `test_ingest.py` against real Postgres + the S3 fake: first-sync window, incremental checkpoints, uidvalidity resync, per-batch commit + stop flag, vanished messages, legacy S3 checkpoint import. The mailbox sits behind the `MailSource` trait (`&mut dyn MailSource`; fakes in tests, `imap::ImapSource` in the worker) |
| `imap.py` | `src/imap.rs` | hand-rolled IMAP4rev1 subset (LOGIN/EXAMINE/UID SEARCH/UID FETCH/LOGOUT, literals both ways) over rustls with the RustCrypto provider (port 993, pure Rust). Pinned by `tests/imap_wire.rs` against a scripted fake server wired through an in-memory pipe — loopback TCP proved unreliable in the sandbox (delayed/empty reads) |
| `logs.py` | `src/logs.rs` | `tests/logs_unit.rs` — JSON/text record shapes, level filtering, a capture sink standing in for `caplog` |
| `worker.py` | `src/worker.rs` | `tests/worker_db.rs` — full port of `test_worker.py` on real Postgres: per-account backoff, live add/remove/change, empty warning once, stop-aware run_forever. SIGTERM/SIGINT land via `src/signals.rs` (handler sets a flag, a watcher copies it into the stop event and the ingest stop flag) |
| `backfill.py` | `src/backfill.rs` | `tests/backfill_db.rs` — unknown keys recorded through the ingestion catalog call, idempotent second run, skipped keys counted |
| `supervise.py` | `src/supervise.rs` | `tests/supervise_unit.rs` — real child processes: one child exiting stops the others and passes its code (0 becomes 1), SIGTERM/SIGINT terminate the children, a SIGTERM-ignoring child is killed after the grace period |
| `cli.py` | `src/cli.rs` + `src/main.rs` | `tests/cli_db.rs` — accounts commands on the S3 fake (add/list/update/disable/remove/set-password/rotate-key, login verification hook), DB admin commands on real Postgres (jobs status/retry, profiles/patterns, set-profile), backfill + jobs end-to-end, and the early exits (config 2, no DATABASE_URL 2, S3 3, migrate 4). `classify` runs the classifier with the TypeSafe transport (`JEV_BASE_URL` overrides the API base the closed SDK hardcodes), `api` serves the HTTP API, `all` supervises the same three services |
| `api.py` | `src/api.rs` | `tests/api_db.rs` — full port of `test_api.py` (24 tests) on real Postgres + the S3 fake, driven through the router function directly (FastAPI's TestClient role): auth gating on every route, /me, /home (time zones via jiff's tzdb, account/day filters, by_hour), /messages paging + filters + cursors, detail with labels.reasons and no s3_key, actions round-trip with changed_by, body from S3, threads with quote stripping, /draft end-to-end through the route (422/404/503 paths included), plus one real-socket smoke test |
| `auth.py` | `src/auth.rs` | `tests/auth_unit.rs` — full port of `test_auth.py`: RS256 verification against minted certificates (hand-rolled minimal X.509 DER walk + `rsa` crate, pure Rust), aud/iss/exp/iat with clock skew, garbage and wrong-key rejections, `authorize` (email verified + allowlist, fail closed), and the Cache-Control max-age certificate cache with a fake clock. The production transport fetches Google's x509 map over HTTPS and was checked against the live endpoint |

Helper binaries (`cargo run --example …`): `dump` (message → meta/body JSON),
`cipher` (Fernet encrypt/decrypt/rotate/keygen), `s3-cli` (list/head/get
against any S3 — validated against the production bucket).

## Building

```sh
tools/dev-postgres.sh start                              # disposable PostgreSQL 16, no root
test -n "$TEST_DATABASE_URL" || eval "$(tools/dev-postgres.sh env)"
cargo test          # unit + golden tests + real-Postgres tests
cargo run --example dump < tests/fixtures/newsletter.eml
```

The binary is built by the same workspace: `cargo build` produces
`target/x86_64-unknown-linux-musl/debug/aimap` (the port of `aimap -m aimap`).

In a sandbox where the musl-hosted rustup proxies cannot run (they need
`/lib/ld-musl-x86_64.so.1`), build with the gnu toolchain for the host and
zig cc as its linker — the target artifacts stay musl-static either way:

```sh
export RUSTUP_TOOLCHAIN=stable-x86_64-unknown-linux-gnu
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$HOME/.cargo/bin/zig-cc-gnu"
```

`.cargo/config.toml` pins the `x86_64-unknown-linux-musl` target linked by
rustup's own `rust-lld` with the self-contained CRT, so **no system gcc is
needed for the target artifacts** and binaries come out fully static.
Consequence: only pure-Rust dependencies are allowed (no C-backed crates).
The Postgres client is the sync `postgres` crate (pure Rust); queue functions
take `&mut impl GenericClient`, so both `Client` and `Transaction` work —
transactions are explicit where the Python code relies on psycopg's implicit
ones.

Build scripts and proc-macros still compile for the host triple; on any normal
machine (and CI) gcc provides the host linker. In a linker-less sandbox, point
the host at `zig cc`:

```sh
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/path/to/zig-cc-gnu cargo test
```

## Golden tests

`tests/fixtures/*.eml` are real-shaped messages; `*.expected.json` are the
values the **Python** implementation produces (`parse_meta`, `derive_thread_id`,
`attachment_names`, `body_text`, optional `body_text_100` / `stripped`).
`tests/parse_golden.rs` asserts the Rust port reproduces them exactly.

To regenerate goldens (e.g. after adding a fixture):

```sh
node tools/make-fixtures.mjs
```

The script also contains a JS transcription of `parse.py` used to derive the
hash- and extraction-dependent expectations. When Python is available, prefer
re-deriving goldens with `src/aimap/parse.py` itself.

## Validation against production mail

The port is validated against every message stored in the live S3 bucket
(908 real Gmail messages, two accounts):

```sh
# pull the stored .eml files (needs the service's S3_* variables)
railway variables --service aimap --json   # from the repo root, after railway link
aws s3 sync "s3://<bucket>/raw/" /tmp/eml --exclude "*/config/*"

# diff Rust vs Python over all of them
PYTHON_BIN=python3 node tools/diff-impls.mjs /tmp/eml
# -> 908 messages: 908 identical, 0 divergent, 0 errors
```

`tools/diff-impls.mjs` runs `tools/dump-python.py` (stdlib-only ground truth
from `src/aimap/parse.py`) and the Rust `dump` example on each message and
compares every meta field and the classifier body, code-point-exact.

Real mail surfaced and fixed three CPython quirks the unit tests alone had
missed, each now pinned by `clean_text_trims_unicode_whitespace_like_python`:

- Python `str.strip()` trims U+2007 (FIGURE SPACE) and `\x1c`-`\x1f` at line
  boundaries (`py_trim` in parse.rs).
- Python `str.splitlines()` splits on `\v \f \x1c-\x1e \x85 U+2028 U+2029`
  (`py_splitlines` in parse.rs). U+2028 appears in real text/plain bodies.

- `classifier.py`'s insert of `priority` goes through `f32` (the column is
  `REAL`; the postgres crate wants an exact type match where psycopg relies on
  implicit float8→float4 coercion).
- `parse_patterns` reports JSON syntax errors as `ProfileError` (Python lets
  `json.JSONDecodeError` propagate); CSV parsing is a hand-rolled reader
  pinned to Python's `csv.reader` defaults, so exotic quoted forms may differ.
- Account/profile validation messages format names with single quotes like
  Python `repr`, but non-string JSON fields are stringified Rust-style
  (`str(True)` → `"True"` is approximated only for booleans).
- Python's `log.*` calls are mirrored record-for-record (message, level,
  extras) by the `Logger` facade in `logs.rs`; tracebacks (`exc_info`) are not
  reproduced, and a few `error` strings describe the failure Rust-side instead
  of echoing Python exception text (`draft model response parse failed`).

The Postgres-backed modules (`db`, `queue`, `catalog`, `inbox`, draft's
orchestration) run against a disposable real Postgres (`tools/dev-postgres.sh`,
or the CI service container) — no embedded replacements. The storage layer
follows the Python pattern: all S3 semantics (key math, conditional writes,
error-code mapping, metadata sanitation) live in `Store` over an injectable
`S3Client`. The real transport (`src/s3.rs`) signs with a hand-rolled SigV4
pinned to a Python-computed signature, and its HTTP flow runs against moto —
the same engine the Python suite uses. HTTP is `src/httpc.rs`, a small
blocking HTTP/1.1 client of the repo's own (ureq's TLS feature hard-depends on
ring, which the static musl build forbids): plain HTTP for moto/SeaweedFS and
HTTPS via the same rustls + rustls-rustcrypto provider the IMAP client uses,
verified against the live Google certificates endpoint. One connection per
request (`Connection: close`); the S3 workload is a few requests per message,
so no pooling.

## Known divergences from CPython

Deliberate, reviewed, and acceptable for this data — tighten a golden if one
bites:

- `casefold` ≈ `to_lowercase` + `ß → ss`; other casefold specials (ligatures)
  are not folded.
- HTML entities must be `;`-terminated (`html-escape` follows strict HTML5;
  Python's `html.unescape` also accepts legacy semicolonless forms).
- `classifier_key`'s canonical JSON formats floats via Rust's serializer;
  values that Python renders as `1e+30` and friends would hash differently.
  Profiles realistically hold strings/ints/simple decimals.
- `build_prompt` treats an explicitly-`None` sender as `(unknown)` (Python's
  `dict.get` default only covers a missing key — `get_thread` always sets it).
- `attachment_names` approximates `iter_attachments`: the first non-attachment
  leaf subpart is treated as the body; everything after it with a filename is
  an attachment. Exotic layouts (nested mixed inside alternative) may differ.
- `get_body` handles alternative/related/mixed but not every CPython corner
  (start-part mismatches, multipart inside text/* bodies).
- RFC 2231 encoded filename params (`filename*=utf-8''…`) are not decoded.
- Address groups (`Group: a@x, b@x;`) and comment forms (`a@x (Ann)`) are not
  parsed like `email.utils.parseaddr`.
- Date parsing uses `mailparse::dateparse`; obsolete zone spellings may
  differ from `email.utils.parsedate_to_datetime` in edge cases.
- The HTTP API runs on a small hand-rolled HTTP/1.1 server (thread per
  connection, keep-alive, JSON responses) instead of FastAPI/uvicorn; request
  lines are logged as one JSON record each (`"message": "request"`). FastAPI's
  schema endpoint `/openapi.json` is not served (404) — Python only disabled
  `/docs`.
- Firebase verification follows Firebase's documented third-party flow:
  RS256 against the `securetoken@system.gserviceaccount.com` x509 map, aud,
  exp/iat with the same 10-second clock skew google-auth applies. Error
  message strings differ from google-auth's (they are 401/403 payloads, not
  pinned anywhere).
- The TypeSafe transport speaks System One directly (the wire protocol the
  closed SDK wraps, cross-checked against pi's reference JS client). The
  recorded `model` is the requested model id, like the JS reference; API error
  strings differ from the Python SDK's, and `JEV_BASE_URL` overrides the base
  URL (Python hardcodes it inside the SDK).
- `days`/`limit`/`filter`/datetime query validation mirrors FastAPI's 422s
  with Rust wording; only the status codes are pinned by the ported tests.
