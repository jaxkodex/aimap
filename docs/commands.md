# Commands

```
aimap all                  run, classify and api as child processes (container default)

aimap run                  poll IMAP forever
aimap once                 one pass, exit 1 if any account failed
aimap check                verify S3 and Postgres, and log in to every enabled account

aimap classify             take classify jobs forever
aimap classify --once      process every due job, then exit
aimap jobs status          job counts per stage and status
aimap jobs retry [--done]  requeue failed jobs, and with --done finished ones too

aimap api                  serve the HTTP API on $PORT

aimap migrate              apply missing schema migrations
aimap backfill             record messages that are in S3 but not in Postgres, and queue them
```

`all` forwards SIGTERM and SIGINT to its children and waits up to 30 seconds
for them to finish. If one child exits, `all` stops the others and exits with
that child's code, or 1 if it was 0, so the platform restarts the container.

`backfill` is for mail stored before the database existed, or for rebuilding
the database from the bucket. It skips keys already recorded, so running it
twice is safe. It does not touch checkpoints.

Account, profile and pattern commands are in [Accounts](accounts.md#commands) and [Profiles and patterns](profiles.md).
