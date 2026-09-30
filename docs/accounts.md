# Accounts

## The accounts file

`config/accounts.json` holds one record per account. Host, port, mailboxes
and the enabled flag are plain JSON, so you can read the file. The password
is a [Fernet](https://cryptography.io/en/latest/fernet/) token, which is
AES-128-CBC with an HMAC-SHA256 tag. It's encrypted with `AIMAP_SECRET_KEY`,
which lives in the service's environment and never in the bucket. Anyone with
read access to the bucket but not the key sees ciphertext only.

Each token encrypts the account's user together with its password. Decryption
checks the user, so a token copied onto another account's record fails.

Writes are conditional on the file's ETag (`If-Match` / `If-None-Match`). If
two people run `aimap accounts` at once, the second write fails, rereads
the file and retries instead of silently overwriting the first change.

## Commands

```sh
aimap keygen                                   # print a new AIMAP_SECRET_KEY

aimap accounts add me@gmail.com --host imap.gmail.com [--port 993] [--mailbox INBOX --mailbox Sent]
aimap accounts list                            # never prints passwords; KEY column shows if it decrypts
aimap accounts update me@gmail.com --mailbox INBOX --mailbox Archive
aimap accounts set-password me@gmail.com
aimap accounts disable me@gmail.com            # stop syncing, keep the record
aimap accounts enable me@gmail.com
aimap accounts remove me@gmail.com             # stored mail stays in the bucket, its rows in Postgres
aimap accounts set-profile me@gmail.com work   # stored in Postgres, not in the file
aimap accounts rotate-key
```

`add` and `set-password` prompt for the password with no echo. In scripts,
pipe it with `--password-stdin`. Neither command accepts the password as an
argument, so it never lands in shell history or `ps`. Before saving, they log
in to the IMAP server and open every mailbox, and they refuse to save if that
fails. `--no-verify` skips the check.

For Gmail, use an [app password](https://myaccount.google.com/apppasswords),
which needs 2-Step Verification. Spaces in pasted app passwords are removed.

## Rotating the key

1. Generate a key and put it first: `AIMAP_SECRET_KEY=<new>,<old>`. Every
   listed key decrypts. The first one encrypts.
2. Run `aimap accounts rotate-key` to re-encrypt every password with `<new>`.
3. Remove `<old>` from `AIMAP_SECRET_KEY` and redeploy.

If the key is lost, the passwords are gone. Set a new key and run
`set-password` for each account.

## Env-only mode

For a single fixed mailbox with no accounts file, set `IMAP_USER`,
`IMAP_PASSWORD` and `IMAP_HOST`. For more accounts, add `IMAP_USER2`,
`IMAP_PASSWORD2` and so on. When `IMAP_USER` is set, `ACCOUNTS_SOURCE`
defaults to `env`. Changes need a restart, and `AIMAP_SECRET_KEY` isn't used.
