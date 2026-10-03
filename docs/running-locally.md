# Run locally

With [uv](https://docs.astral.sh/uv/):

```sh
uv sync
cp .env.example .env                           # S3, DATABASE_URL, JEV_API_KEY
echo "AIMAP_SECRET_KEY=$(uv run aimap keygen)" >> .env
uv run aimap migrate
uv run aimap accounts add me@gmail.com --host imap.gmail.com
uv run aimap all                               # needs JEV_API_KEY, FIREBASE_PROJECT_ID and AIMAP_ALLOWED_EMAILS
```

`aimap all` runs the worker, the classifier and the API together. To run
them one at a time, use `aimap run`, `aimap classify` and `aimap api` in
separate terminals.

Without AWS, Docker Compose starts Postgres and [SeaweedFS](https://github.com/seaweedfs/seaweedfs)
as a local S3 store, creates the bucket, applies the migrations and runs all
three processes, each in its own container, with the API on http://localhost:8080:

```sh
echo "AIMAP_SECRET_KEY=$(docker run --rm $(docker build -q .) keygen)" > .env
echo "JEV_API_KEY=..." >> .env
echo "FIREBASE_PROJECT_ID=..." >> .env
echo "AIMAP_ALLOWED_EMAILS=me@gmail.com" >> .env
docker compose up --build -d
docker compose run --rm -it worker accounts add me@gmail.com --host imap.gmail.com
docker compose run --rm -T worker profiles set default --file - < profile.json
docker compose logs -f worker classifier       # picks the account up within a minute
```

Every variable is listed in [Configuration](configuration.md).
