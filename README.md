# aimap

Watches one or more IMAP mailboxes, copies every new message byte for byte
into an S3 bucket, and classifies it with [TypeSafe Jev](https://typesafe.ai):
importance, a suggested action and tags. Message metadata and labels go to
Postgres. Bodies stay in S3 only.

It is one Docker image with three processes:

- `aimap run` polls IMAP, writes `.eml` files to S3, records their headers in
  Postgres and queues a `classify` job for each new message.
- `aimap classify` takes those jobs, reads the message from S3, asks Jev and
  stores the labels.
- `aimap api` serves the labelled mail over HTTP to the app, for users signed
  in with Firebase Authentication, and records what the user did with a
  message (handled, later). The mailbox itself is only ever read.

They share nothing but the bucket and the database, so a Jev outage never
stops ingestion. `aimap all`, the image's default, runs the three as child
processes of one container, and stops the container if any of them exits.

Accounts live in an encrypted file in the same bucket. You add or change
them with `aimap accounts ...`, and the running worker picks the change up on
its next poll.

## Quick start

```sh
uv sync
cp .env.example .env                           # S3, DATABASE_URL, JEV_API_KEY
echo "AIMAP_SECRET_KEY=$(uv run aimap keygen)" >> .env
uv run aimap migrate
uv run aimap accounts add me@gmail.com --host imap.gmail.com
uv run aimap all                               # or `aimap run`, `aimap classify` and `aimap api` separately
```

[Run locally](docs/running-locally.md) also covers Docker Compose without AWS.

## Documentation

How it works:

- [Ingestion](docs/ingestion.md): polling IMAP, checkpoints, backoff.
- [Classification](docs/classification.md): the job queue, the Jev request, retries.
- [Profiles and patterns](docs/profiles.md): what Jev classifies against.
- [Storage](docs/storage.md): the bucket layout and the Postgres tables.
- [HTTP API](docs/api.md): the routes the app uses, and Firebase sign-in.

Running it:

- [Accounts](docs/accounts.md): the encrypted accounts file, commands, key rotation.
- [Configuration](docs/configuration.md): every environment variable.
- [Commands](docs/commands.md): the `aimap` CLI.
- [Run locally](docs/running-locally.md) and [Deploy on Railway](docs/deploy-railway.md).
- [Development](docs/development.md): tests and linting.

## Roadmap

- [x] IMAP to S3 ingestion loop with checkpoints
- [x] Encrypted, live-reloaded accounts file
- [ ] OAuth2 (XOAUTH2) refresh tokens as an alternative to app passwords
- [x] Message metadata, checkpoints and a job queue in Postgres
- [x] Classify each message with [TypeSafe Jev](https://typesafe.ai): importance,
      action bucket, tags, priority
- [x] Profiles and pattern catalogues in Postgres, one profile per account
- [x] HTTP API for the app, behind Firebase Authentication. It reads the
      mailbox only; the one write is aimap's own Handled / Later state
- [ ] Embeddings: an `embed` job stage that reads the `.eml` from S3 and writes
      to a pgvector table (Railway needs its pgvector Postgres image for this)
- [ ] Re-run `decide` over stored Jev answers after a threshold change, with
      no new Jev calls
- [ ] Feedback loop: turn corrected labels into patterns

## License

[MIT](LICENSE)
