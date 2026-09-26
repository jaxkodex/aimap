-- Accounts, checkpoints, message metadata, the job queue and classifications.
-- No message bodies are stored here; the raw .eml stays in S3 (message_locations.s3_key).

-- A profile is the recipient context Jev classifies against ("who am I, what matters").
-- One person can have several (for example work and personal). Each account uses one.
CREATE TABLE profiles (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    profile     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO profiles (name) VALUES ('default');

-- Credentials stay in the encrypted accounts file in S3. This row only gives the
-- address an id and a profile. The worker upserts it on every accounts reload.
CREATE TABLE accounts (
    id          BIGSERIAL PRIMARY KEY,
    address     TEXT NOT NULL UNIQUE,
    profile_id  BIGINT NOT NULL REFERENCES profiles(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- IMAP checkpoint per mailbox. Replaces state/<account>/<mailbox>.json in S3.
CREATE TABLE mailbox_state (
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox      TEXT NOT NULL,
    uidvalidity  BIGINT NOT NULL,
    last_uid     BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, mailbox)
);

-- One row per distinct message in an account, keyed by its RFC 822 Message-ID
-- (or "sha256:<hex>" of the raw bytes when the header is missing). The same message
-- in INBOX and [Gmail]/All Mail is one row with two locations.
CREATE TABLE messages (
    id                 BIGSERIAL PRIMARY KEY,
    account_id         BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    rfc822_message_id  TEXT NOT NULL,
    from_email         TEXT,
    from_name          TEXT,
    subject            TEXT,
    sent_at            TIMESTAMPTZ,
    in_reply_to        TEXT,
    has_list_headers   BOOLEAN NOT NULL DEFAULT false,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, rfc822_message_id)
);

CREATE INDEX messages_account_sent_idx ON messages (account_id, sent_at DESC);

-- Where a message is stored: one row per (mailbox, uidvalidity, uid) and its S3 key.
CREATE TABLE message_locations (
    id             BIGSERIAL PRIMARY KEY,
    message_id     BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id     BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox        TEXT NOT NULL,
    uidvalidity    BIGINT NOT NULL,
    uid            BIGINT NOT NULL,
    s3_key         TEXT NOT NULL UNIQUE,
    flags          TEXT[] NOT NULL DEFAULT '{}',
    internal_date  TIMESTAMPTZ,
    ingested_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, mailbox, uidvalidity, uid)
);

CREATE INDEX message_locations_message_idx ON message_locations (message_id);

-- Work queue. One row per (message, stage). 'classify' today, 'embed' later.
-- Workers claim rows with SELECT ... FOR UPDATE SKIP LOCKED.
CREATE TABLE jobs (
    id          BIGSERIAL PRIMARY KEY,
    message_id  BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    stage       TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'running', 'done', 'failed')),
    attempts    INT NOT NULL DEFAULT 0,
    run_after   TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at   TIMESTAMPTZ,
    last_error  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (message_id, stage)
);

CREATE INDEX jobs_pending_idx ON jobs (stage, run_after) WHERE status = 'pending';
CREATE INDEX jobs_running_idx ON jobs (stage, locked_at) WHERE status = 'running';

-- Known inbox patterns per profile. A matching pattern's labels are copied as is.
CREATE TABLE patterns (
    id                BIGSERIAL PRIMARY KEY,
    profile_id        BIGINT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    insight           TEXT NOT NULL,
    importance        TEXT NOT NULL CHECK (importance IN ('Low', 'Medium', 'High')),
    action_bucket     TEXT NOT NULL,
    tags              TEXT[] NOT NULL DEFAULT '{}',
    examples          JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (profile_id, insight)
);

-- Jev output. raw_answers keeps the full response so the decision can be recomputed
-- without paying for inference again. classifier_key hashes the questions, model and
-- profile, so changing any of them adds a new row instead of overwriting.
CREATE TABLE classifications (
    id               BIGSERIAL PRIMARY KEY,
    message_id       BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    profile_id       BIGINT NOT NULL REFERENCES profiles(id),
    classifier_key   TEXT NOT NULL,
    model            TEXT NOT NULL,
    raw_answers      JSONB NOT NULL,
    usage            JSONB,
    importance       TEXT NOT NULL,
    action_bucket    TEXT NOT NULL,
    tags             TEXT[] NOT NULL DEFAULT '{}',
    insight          TEXT,
    source           TEXT NOT NULL CHECK (source IN ('pattern', 'judgment')),
    needs_review     BOOLEAN NOT NULL,
    priority         REAL NOT NULL,
    signals          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (message_id, classifier_key)
);

CREATE INDEX classifications_message_idx ON classifications (message_id, created_at DESC);

-- Latest classification per message, joined with its metadata.
CREATE VIEW message_labels AS
SELECT DISTINCT ON (c.message_id)
    m.id AS message_id, a.address AS account, m.from_email, m.from_name, m.subject, m.sent_at,
    c.importance, c.action_bucket, c.tags, c.insight, c.source, c.needs_review, c.priority,
    c.model, c.created_at AS classified_at
FROM classifications c
JOIN messages m ON m.id = c.message_id
JOIN accounts a ON a.id = m.account_id
ORDER BY c.message_id, c.created_at DESC, c.id DESC;
