# Storage

Message bodies live in the S3 bucket, everything else in Postgres.

## Bucket layout

```
<S3_PREFIX>/
  config/accounts.json                              accounts, passwords encrypted
  raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml   Content-Type message/rfc822
                                                    metadata: imap-flags, imap-internaldate
  state/<account>/<mailbox>.json                    legacy checkpoints, read once on upgrade
```

Mailbox names are URL-encoded, so `[Gmail]/All Mail` becomes `%5BGmail%5D%2FAll%20Mail`.

## Database

| Table | One row per |
|---|---|
| `profiles` | Recipient context Jev classifies against. `default` exists from the start. |
| `accounts` | Address from the accounts file, with its profile. No credentials. |
| `mailbox_state` | Account and mailbox: `uidvalidity`, `last_uid`. |
| `messages` | Account and RFC 822 Message-ID: sender, subject, date, reply and list headers. A message without a Message-ID uses `sha256:<hash of the raw bytes>`. |
| `message_locations` | Mailbox, UIDVALIDITY and UID where a message is stored, with its S3 key and IMAP flags. The same message in `INBOX` and `[Gmail]/All Mail` has two locations and one `messages` row, so it is classified once. |
| `jobs` | Message and stage. The only stage today is `classify`. |
| `patterns` | Known pattern of a profile: insight, importance, action bucket, tags, example senders and subjects. |
| `classifications` | Message and classifier key: labels, `needs_review`, `priority`, signals, and the raw Jev answers. |
| `message_state` | Message you marked `handled` or `later` in the app, with when and which verified email did it. Undo deletes the row, so no row means normal. Nothing here is sent to IMAP. |

The `message_labels` view joins each message with its latest classification.

`aimap migrate` applies the numbered SQL files in
[`src/aimap/migrations`](../src/aimap/migrations) that are missing, each in its
own transaction, under an advisory lock. `run` and `classify` exit at startup
if a migration is missing. To change the schema, add the next numbered file.
Never edit one that has shipped.
