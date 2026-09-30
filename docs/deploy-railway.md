# Deploy on Railway

1. Create a project and add a service from this GitHub repo. Railway reads
   [`.railway/railway.ts`](../.railway/railway.ts), which defines the `aimap`,
   `classify` and `api` services, and builds the `Dockerfile`. This is the ingest worker.
2. Add a bucket to the project, or use any S3-compatible bucket you already
   have. Add a Postgres database.
3. On the service, set `S3_BUCKET`, `S3_ENDPOINT_URL`, `S3_REGION`,
   `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`. If you use a Railway bucket,
   reference its variables instead of pasting values, e.g.
   `S3_BUCKET=${{Bucket.BUCKET}}`. The names on the right come from the
   bucket's Variables tab. Set `DATABASE_URL=${{Postgres.DATABASE_URL}}`.
4. Set `AIMAP_SECRET_KEY` to the output of `aimap keygen`. Also store it in
   your password manager.
5. Deploy. Each service runs `aimap migrate` before its deploy starts. The
   service needs no public domain or volume. The logs show
   `no accounts configured; waiting` until you add one.
6. Add a second service from the same repo for the classifier. Give it the
   same S3 and `DATABASE_URL` variables, plus `JEV_API_KEY`, and set its
   start command to `aimap classify`. It does not need `AIMAP_SECRET_KEY`.
   To classify faster, raise `CLASSIFY_CONCURRENCY` or add replicas. Jobs are
   claimed with row locks, so replicas never share one.
7. Add a third service from the same repo for the API, with start command
   `aimap api`. Give it the same S3 and `DATABASE_URL` variables, plus
   `FIREBASE_PROJECT_ID` and `AIMAP_ALLOWED_EMAILS`. It needs neither
   `AIMAP_SECRET_KEY` nor `JEV_API_KEY`. Generate a public domain for it and
   set its healthcheck path to `/healthz`. Railway sets `PORT`.
8. Add accounts from your machine with the service's variables injected. The
   bucket endpoint has to be reachable from where you run this:

   ```sh
   railway link                                # pick the project and service
   railway run uv run aimap accounts add me@gmail.com --host imap.gmail.com
   ```

   Or open a shell in the running container with `railway ssh` and run
   `aimap accounts add ...` there.

The worker logs `account added` and `first sync` within one poll interval,
with no redeploy. Set a profile with
`railway run uv run aimap profiles set default --file profile.json`. If the
bucket already holds mail from before Postgres, run `aimap backfill` once to
queue it.
