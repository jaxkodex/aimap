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
| `GET /messages/{id}` | One message: metadata, labels, signals, reasons and mailboxes. |
| `GET /messages/{id}/body` | The text body, read from S3 for that request and not kept. |
| `POST /messages/{id}/actions` | Marks a message handled or later, or undoes it. Returns its new state. |

`/home` takes `account`, `days` (the window, default 7), `new_since` (for
the brief's `new` count, default 24 hours ago) and `tz`. `act_now` holds the `act_now`
and `verify` buckets, highest `priority` first. `waiting` holds `reply`, the
longest waiting first. Everything else is grouped under its pattern's insight,
its first tag or its bucket. Reasons ("Mentions a deadline") and group
summaries ("Uber, AWS Billing + 1 more") are templates over stored signals and
senders, so the API never calls Jev. Mail the account sent itself is left out.
Each card carries its account's `profile` name ("work", "personal"), which the
app can show as a short account label.

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
