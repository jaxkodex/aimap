-- Add thread_id for grouping messages into conversations. The thread_id is the root
-- of the References/In-Reply-To chain when present, else a hash of the normalized
-- subject (stripping RE:/FW: etc.) plus sorted participant addresses.

ALTER TABLE messages ADD COLUMN message_references TEXT;
ALTER TABLE messages ADD COLUMN thread_id TEXT;

-- Backfill thread_id for existing rows. Derivation: use the leftmost message-id from
-- the References header or the In-Reply-To value when no References exist, or hash the
-- normalized subject with sorted participants (from the account and from_email).
-- The subject normalizer strips RE:/FW:/FWD: and localized variants (AW:, TR:, SV:,
-- ENC:, RIF:, RES:, WG:, ANTW:, VS:, YNT:), collapses whitespace, and casefolds.

CREATE OR REPLACE FUNCTION _normalize_subject(subj TEXT) RETURNS TEXT AS $$
DECLARE
    s TEXT := coalesce(subj, '');
    prev TEXT;
BEGIN
    LOOP
        prev := s;
        s := regexp_replace(s, '^\s*(re|fw|fwd|aw|r|tr|sv|enc|rif|res|wg|antw|vs|ynt)\s*:\s*', '', 'gi');
        EXIT WHEN s = prev;
    END LOOP;
    s := regexp_replace(s, '\s+', ' ', 'g');
    s := trim(s);
    RETURN lower(s);
END;
$$ LANGUAGE plpgsql IMMUTABLE;

-- Build a thread_id from message_references, in_reply_to, subject, and participants.
-- Returns the leftmost message-id from the chain, or a hash when no chain exists.
UPDATE messages SET thread_id = t.computed_thread_id
FROM (
    SELECT m.id,
        CASE
            WHEN m.message_references IS NOT NULL AND m.message_references ~ '<[^>]+>' THEN
                (regexp_match(m.message_references, '<[^>]+>'))[1]
            WHEN m.in_reply_to IS NOT NULL AND m.in_reply_to ~ '^<.*>$' THEN
                m.in_reply_to
            WHEN m.rfc822_message_id IS NOT NULL AND m.rfc822_message_id ~ '^<.*>$' THEN
                m.rfc822_message_id
            ELSE
                'thread:' || substring(md5(
                    coalesce(_normalize_subject(m.subject), '') || '|' ||
                    coalesce(string_agg(lower(p.addr), '|' ORDER BY p.addr), '')
                ), 1, 16)
        END AS computed_thread_id
    FROM messages m
    JOIN accounts a ON a.id = m.account_id
    CROSS JOIN LATERAL (
        SELECT addr FROM (
            VALUES (a.address), (m.from_email)
        ) AS v(addr)
        WHERE addr IS NOT NULL
    ) p
    GROUP BY m.id, m.message_references, m.in_reply_to, m.rfc822_message_id, m.subject, a.address
) t
WHERE messages.id = t.id;

DROP FUNCTION _normalize_subject;

ALTER TABLE messages ALTER COLUMN thread_id SET NOT NULL;
CREATE INDEX messages_thread_id_idx ON messages (thread_id);
