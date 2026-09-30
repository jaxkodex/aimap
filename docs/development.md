# Development

```sh
uv sync
docker run -d --rm --name aimap-pg -p 55432:5432 -e POSTGRES_PASSWORD=pg postgres:16
TEST_DATABASE_URL=postgresql://postgres:pg@localhost:55432/postgres uv run pytest
uv run ruff check .
```

Tests use [moto](https://github.com/getmoto/moto) for S3, an in-memory fake
mailbox and a fake Jev client. They need no network access and no
credentials. Tests that need Postgres create a throwaway database next to
`TEST_DATABASE_URL`, apply the migrations and drop it at the end. Without
`TEST_DATABASE_URL` they are skipped, except in CI, where they fail.
