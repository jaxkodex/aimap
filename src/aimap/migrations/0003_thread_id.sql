-- Add thread_id for grouping messages into conversations. The thread_id is the root
-- of the References/In-Reply-To chain when present, else a hash of the normalized
-- subject (stripping RE:/FW: etc.) plus sorted participant addresses.

ALTER TABLE messages ADD COLUMN references TEXT;
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

UPDATE messages SET thread_id = CASE
    -- References header: split on commas and whitespace, take leftmost <message-id>
    WHEN references IS NOT NULL AND references ~ '<[^>]+>' THEN
        (SELECT (regexp_matches(references, '<[^>]+>', 'g'))[1] LIMIT 1)
    -- In-Reply-To: use it if it looks like a message-id
    WHEN in_reply_to IS NOT NULL AND in_reply_to ~ '^<.*>$' THEN
        in_reply_to
    -- No reply chain: hash normalized subject + sorted participants (account address, from_email)
    ELSE
        'thread:' || substring(encode(digest(
            _normalize_subject(subject) || '|' ||
            array_to_string(array_agg(lower(p) ORDER BY p) FILTER (WHERE p IS NOT NULL), '|', ''),
            'sha256'
        ), 'hex'), 1, 16)
END
FROM (
    SELECT m2.id, a.address AS account_addr, m2.from_email
    FROM messages m2
    JOIN accounts a ON a.id = m2.account_id
) parts
WHERE messages.id = parts.id;

DROP FUNCTION _normalize_subject;

ALTER TABLE messages ALTER COLUMN thread_id SET NOT NULL;
CREATE INDEX messages_thread_id_idx ON messages (thread_id);
