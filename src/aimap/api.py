"""HTTP API for the app: `aimap api`. Read-only over Postgres, bodies from S3 on request.

Every route except /healthz needs a Firebase ID token (see auth.py). Handlers are
plain functions, so FastAPI runs them in its thread pool and they share the
psycopg connection pool the same way the classifier threads do.
"""

from __future__ import annotations

import logging
from datetime import UTC, datetime, timedelta
from typing import Annotated, Literal

from fastapi import Depends, FastAPI, HTTPException, Query, Request
from fastapi.responses import JSONResponse
from psycopg_pool import ConnectionPool

from aimap import __version__, home, inbox
from aimap.auth import AuthError, Forbidden, User, Verifier
from aimap.parse import body_text
from aimap.storage import Store

log = logging.getLogger(__name__)

BODY_CHARS = 50_000


def current_user(request: Request) -> User:
    scheme, _, token = request.headers.get("authorization", "").partition(" ")
    if scheme.lower() != "bearer" or not token:
        raise HTTPException(401, "missing bearer token", headers={"WWW-Authenticate": "Bearer"})
    verifier: Verifier = request.app.state.verifier
    try:
        return verifier.verify(token.strip())
    except Forbidden as e:
        log.warning("api access denied", extra={"reason": str(e)})
        raise HTTPException(403, "not allowed") from e
    except AuthError as e:
        raise HTTPException(401, "invalid token", headers={"WWW-Authenticate": "Bearer"}) from e


# Module level, not inside create_app: FastAPI resolves these annotations by name.
Authed = Annotated[User, Depends(current_user)]


def create_app(pool: ConnectionPool, store: Store, verifier: Verifier) -> FastAPI:
    app = FastAPI(title="aimap", version=__version__, docs_url=None, redoc_url=None)

    app.state.verifier = verifier

    @app.exception_handler(inbox.CursorError)
    def bad_cursor(_request: Request, e: inbox.CursorError) -> JSONResponse:
        return JSONResponse({"detail": str(e)}, status_code=400)

    @app.get("/healthz")
    def healthz() -> dict:
        with pool.connection() as conn:
            conn.execute("SELECT 1")
        return {"ok": True}

    @app.get("/me")
    def me(user: Authed) -> dict:
        return {"uid": user.uid, "email": user.email}

    @app.get("/accounts")
    def accounts(_user: Authed) -> dict:
        with pool.connection() as conn:
            return {"accounts": inbox.accounts(conn)}

    @app.get("/home")
    def home_screen(
        _user: Authed,
        account: str | None = None,
        days: Annotated[int, Query(ge=1, le=90)] = 7,
        new_since: datetime | None = None,
    ) -> dict:
        """Sections over the last `days` days. `new` in the brief counts mail since `new_since` (default 24h)."""
        now = datetime.now(UTC)
        with pool.connection() as conn:
            items = inbox.home_items(conn, now - timedelta(days=days), account)
        return home.build(items, new_since or now - timedelta(hours=24))

    @app.get("/messages")
    def messages(
        _user: Authed,
        account: str | None = None,
        filter: Literal["all", "unread", "flagged", "needs_review"] = "all",
        cursor: str | None = None,
        limit: Annotated[int, Query(ge=1, le=200)] = 50,
    ) -> dict:
        with pool.connection() as conn:
            items, next_cursor = inbox.list_messages(conn, account=account, filter=filter, cursor=cursor,
                                                     limit=limit)
        return {"messages": items, "next_cursor": next_cursor}

    def _message(message_id: int) -> dict:
        with pool.connection() as conn:
            m = inbox.get_message(conn, message_id)
        if m is None:
            raise HTTPException(404, "no such message")
        return m

    @app.get("/messages/{message_id}")
    def message(_user: Authed, message_id: int) -> dict:
        m = _message(message_id)
        m.pop("s3_key")
        if m["labels"]:
            m["labels"]["reasons"] = home.reasons(m["labels"]["signals"])
        return m

    @app.get("/messages/{message_id}/body")
    def message_body(_user: Authed, message_id: int) -> dict:
        """The text body, read from S3 for this request only. Nothing is cached or stored."""
        key = _message(message_id)["s3_key"]
        got = store.get_raw(key) if key else None
        if got is None:
            raise HTTPException(404, "message body is not in the bucket")
        return {"message_id": message_id, "text": body_text(got[0], BODY_CHARS)}

    return app


def serve(app: FastAPI, host: str, port: int, log_level: str) -> None:
    import uvicorn

    # log_config=None keeps the handlers logs.setup installed, so access lines are JSON too.
    uvicorn.run(app, host=host, port=port, log_config=None, log_level=log_level.lower(), proxy_headers=True,
                forwarded_allow_ips="*")
