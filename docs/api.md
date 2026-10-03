# HTTP API

`aimap api` listens on `$PORT` (default 8080). It writes one thing: aimap's own
record of what you did with a message (handled, later). The mailbox, the bucket
and the labels are never changed, and IMAP stays read-only.

| Route | Returns |
|---|---|
| `GET /healthz` | `{"ok": true}` when Postgres answers. No token needed. |
| `GET /me` | The signed-in user's Firebase uid and email. |
| `GET /accounts` | Each account with its profile, message count and unread count. |
| `GET /home` | The Home screen: a brief, `act_now`, `waiting` and `sorted` sections. |
| `GET /messages` | Messages newest first, with their latest labels. |
| `GET /messages/{id}` | One message: metadata, labels, signals, reasons, mailboxes, state and reply info. |
| `GET /messages/{id}/body` | The text body, read from S3 for that request and not kept. |
| `GET /messages/{id}/thread` | Messages in the same thread, newest first, with excerpts. |
| `POST /messages/{id}/actions` | Marks a message handled or later, or undoes it. Returns its new state. |
| `POST /messages/{id}/draft` | Generates a draft reply by reading the thread and calling a model. |

`/home` takes `account`, `days` (the window, default 7), `new_since` (for
the brief's `new` count, default 24 hours ago) and `tz`. `act_now` holds the `act_now`
and `verify` buckets, highest `priority` first. `waiting` holds `reply`, the
longest waiting first. Everything else is grouped under its pattern's insight,
its first tag or its bucket. Reasons ("Mentions a deadline") and group
summaries ("Uber, AWS Billing + 1 more") are templates over stored signals and
senders, so the API never calls Jev. Mail the account sent itself is left out.
Each card carries its account's `profile` name ("work", "personal"), which the
app can show as a short account label. `GET /messages/{id}` carries the same
`profile`, next to its `account`, so the detail screen can show that label too.

A message you marked handled is left out of `act_now` and `waiting` and of
their brief counts. One pushed to later stays in its section but sorts after
every other card. Each card carries `state`: `null` or `"later"`.

The brief also has `sorted_at`, when the newest classification was stored,
`handled_today`, how many messages you handled since local midnight in `tz`, and
`by_hour`, today's traffic: 24 rows, one per hour, each counting the messages
that arrived in `act_now`, `waiting`, `sorted` and `unclassified`. "Today" and
its hours follow `tz`, an IANA time zone name such as `Europe/Madrid` (default
`UTC`). An unknown zone is a 400.

`/messages` takes `account`, `filter` (`all`, `unread`, `flagged`,
`needs_review`), `limit` (up to 200) and `cursor`. Pass the response's
`next_cursor` to get the next page. It is `null` on the last one.

Unread and flagged come from the IMAP flags seen at ingestion. Mail read in
another client still shows as unread. `waiting` means Jev thinks someone
expects an answer, not that no reply was sent: Sent mail isn't matched yet.

## Handled and later

```
POST /messages/12/actions      {"action": "handled" | "later" | "undo"}
200                            {"message_id": 12, "state": "handled", "changed_at": "2026-09-28T09:00:00Z"}
```

`handled` takes the message off `act_now` and `waiting`. `later` keeps it where
it is but sends it to the end of its section, and because every call refreshes
`changed_at`, the message marked later most recently sorts last. `undo` drops
the state, so `state` and `changed_at` come back `null`.

The route is idempotent: sending the same action twice gives the same answer.
An unknown id is a 404 and anything other than those three actions is a 422.
The state is stored in Postgres with the verified email that set it, and
`GET /messages/{id}` returns it as `state`: `null`, `"handled"` or `"later"`.
Nothing is written to IMAP, so archiving, reading or labelling mail in another
client is unaffected.

## Reply detection

`GET /messages/{id}` includes a `reply` object when the message has been classified:

```json
{
  "message_id": 4211,
  "reply": {"needed": true, "reason": "A person asked you to reschedule."},
  "...": "other fields unchanged"
}
```

The `reply` field is absent when the message has no labels yet. The app treats
an absent `reply` as a signal to fall back to a local heuristic, so older
service versions keep working.

Reply detection reads labels already stored and never calls a model. A message
needs a reply when its `action_bucket` is `"reply"`, or when it is `"act_now"`
with the `real_person` signal and without bulk list headers. The `reason` is
the first signal-based reason template, or a fallback string.

## Thread and draft

`GET /messages/{id}/thread` returns messages in the same conversation, newest
first, with excerpts from their bodies:

```json
{
  "message_id": 4211,
  "thread_id": "9f2c...",
  "messages": [
    {
      "message_id": 4211,
      "sender": "Barron, Javier",
      "from_email": "javier.barron@solera.com",
      "subject": "RE: Jorge - Solera Interview - SW Mgr",
      "sent_at": "2025-06-02T00:42:00Z",
      "from_recipient": false,
      "excerpt": "Jorge, by any chance can you please re schedule..."
    }
  ]
}
```

The `thread_id` is derived from the References or In-Reply-To headers when
present, or a hash of the normalized subject and sorted participant addresses.
The `excerpt` is the first ~240 characters of the body with quoted blocks and
signatures removed. It is `null` when the body is no longer in the bucket.
`from_recipient` is true when the account's own address wrote the message.

`POST /messages/{id}/draft` generates a reply by reading the thread and calling
an OpenAI-compatible chat completions endpoint:

```json
{"instructions": "Say Tuesday or Wednesday after 15:00 works."}
```

Returns 200 with:

```json
{
  "message_id": 4211,
  "needs_reply": true,
  "reply_reason": "A person asked you to reschedule.",
  "to": ["javier.barron@solera.com"],
  "cc": [],
  "subject": "RE: Jorge - Solera Interview - SW Mgr",
  "body": "Hi Javier,\n\nTuesday or Wednesday after 15:00 both work...",
  "model": "deepseek-chat",
  "used_message_ids": [4211, 4188],
  "created_at": "2025-06-02T07:10:00Z"
}
```

The `instructions` field is optional and capped at 2000 characters. The `body`
is plain text with no quoted original and no subject line inside it. The
`subject` has the `RE:` prefix added once, never doubled. The `used_message_ids`
list shows which messages the model actually saw, newest first, capped at 6
messages or 12000 characters total.

Drafts are not stored. Calling the route twice calls the model twice. Errors:

- 404 when the message is unknown.
- 422 when `instructions` is too long.
- 503 when drafting is not configured (no API key or model). Detail:
  "Drafting is not configured."
- 502 when the model refused or timed out. The detail says which, in one
  sentence, with no key material and no raw provider payload.

Configure drafting with the `AIMAP_DRAFT_*` variables. Unset `AIMAP_DRAFT_API_KEY`
turns the feature off. Both routes still answer, but `/draft` returns 503.

## Sign-in

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

The variables `aimap api` needs are in [Configuration](configuration.md).
