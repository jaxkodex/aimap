# syntax=docker/dockerfile:1
# The Python image, kept until the Python package is removed. Production
# (`.railway/railway.ts`) and docker-compose now build rust/Dockerfile, the
# port of this stack as a static Rust binary.
FROM python:3.12-slim AS build
COPY --from=ghcr.io/astral-sh/uv:0.8 /uv /bin/uv
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never
WORKDIR /app
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev --no-install-project
COPY README.md ./
COPY src ./src
RUN uv sync --frozen --no-dev --no-editable

FROM python:3.12-slim
RUN useradd --system --uid 10001 --home /app aimap
COPY --from=build --chown=aimap:aimap /app/.venv /app/.venv
ENV PATH="/app/.venv/bin:$PATH" PYTHONUNBUFFERED=1
WORKDIR /app
USER aimap
ENTRYPOINT ["aimap"]
CMD ["all"]
