# Deploy on Railway

One service runs everything. Its start command is `aimap all`, which runs the
ingest worker, the classifier and the API as child processes. If any of them
exits, the container stops and Railway restarts it.

1. Create a project and add a service from this GitHub repo. Railway reads
   [`.railway/railway.ts`](../.railway/railway.ts), which defines the `aimap`
   service, builds the `Dockerfile` and sets the `/healthz` healthcheck.
2. Add a bucket to the project, or use any S3-compatible bucket you already
   have. Add a Postgres database.
3. On the service, set `S3_BUCKET`, `S3_ENDPOINT_URL`, `S3_REGION`,
   `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`. If you use a Railway bucket,
   reference its variables instead of pasting values, e.g.
   `S3_BUCKET=${{Bucket.BUCKET}}`. The names on the right come from the
   bucket's Variables tab. Set `DATABASE_URL=${{Postgres.DATABASE_URL}}`.
4. Set `AIMAP_SECRET_KEY` to the output of `aimap keygen`. Also store it in
   your password manager.
5. Set `JEV_API_KEY` for the classifier, and `FIREBASE_PROJECT_ID` and
   `AIMAP_ALLOWED_EMAILS` for the API. If any of these is missing, its child
   process exits at startup and takes the service down with it, so the deploy
   fails instead of half running.
6. Generate a public domain for the service. Railway sets `PORT`.
7. Deploy. The service runs `aimap migrate` before each deploy starts. It
   needs no volume. The logs show `no accounts configured; waiting` until you
   add one.
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

Run one replica only: each replica would also poll IMAP. To classify faster,
raise `CLASSIFY_CONCURRENCY`. The three processes share the service's CPU and
memory limit.

To run the processes as separate services instead, deploy the same image three
times with start commands `aimap run`, `aimap classify` and `aimap api`. Then
the API needs neither `AIMAP_SECRET_KEY` nor `JEV_API_KEY`, and the classifier
can have replicas.
