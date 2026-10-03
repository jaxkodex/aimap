-- What the user did with a message in the app: handled, or pushed to later.
-- aimap's own state. The mailbox, the bucket and the labels are never touched;
-- IMAP stays read-only. Undo deletes the row, so "no row" means "normal".
CREATE TABLE message_state (
    message_id  BIGINT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    state       TEXT NOT NULL CHECK (state IN ('handled', 'later')),
    changed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    changed_by  TEXT NOT NULL            -- verified Firebase email
);

-- The brief's handled_today counts rows since local midnight.
CREATE INDEX message_state_changed_idx ON message_state (state, changed_at DESC);
