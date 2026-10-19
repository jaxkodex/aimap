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
| `db.py` | `src/db.rs` | `tests/queue_db.rs` against real Postgres; migration SQL embedded from `src/aimap/migrations` via `include_dir!` |
| `queue.py` | `src/queue.rs` | `tests/queue_db.rs` on real Postgres, incl. the 4-thread claim race and SKIP LOCKED semantics |
| `storage.py` | `src/storage.rs` + `src/s3.rs` | `tests/storage_unit.rs` (in-memory fake) + `tests/s3_transport.rs`: SigV4 golden vs a Python-computed signature, live HTTP integration against moto |
| `catalog.py` | `src/catalog.rs` | `tests/catalog_db.rs` on real Postgres (idempotent batches, one-message-two-mailboxes, atomic rollback) |
| `home.py` | `src/home.rs` | `tests/home_unit.rs` — full port of `test_home.py` |
| `inbox.py` | `src/inbox.rs` | `tests/inbox_draft_db.rs` (needs_reply, cursors, paging, get_message/set_state/get_thread on real Postgres) |
| `classifier.py` | — | needs config + a connection pool + wiring; all the `jev::decide` logic is already here |
| `imap.py`, `ingest.py`, `accounts.py`, `config.py`, `worker.py`, `supervise.py` | — | remaining |
| `api.py`, `auth.py` | — | last (axum + Firebase JWT; HTTPS via a pure-Rust rustls provider) |

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

The Postgres-backed modules (`db`, `queue`, `catalog`, `inbox`, draft's
orchestration) run against a disposable real Postgres (`tools/dev-postgres.sh`,
or the CI service container) — no embedded replacements. The storage layer
follows the Python pattern: all S3 semantics (key math, conditional writes,
error-code mapping, metadata sanitation) live in `Store` over an injectable
`S3Client`. The real transport (`src/s3.rs`) signs with a hand-rolled SigV4
pinned to a Python-computed signature, and its HTTP flow runs against moto —
the same engine the Python suite uses. TLS is the one open gap: ureq runs
without a TLS backend, so the transport speaks plain HTTP (moto/SeaweedFS
accept it; AWS us-east-1 too — the production bucket was listed and read with
it); HTTPS lands with the API batch via the pure-Rust rustls +
rustls-rustcrypto provider.

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
