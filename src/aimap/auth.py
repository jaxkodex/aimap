"""Who may call the API: a Firebase Authentication ID token for an allowed email.

The app signs in with the Firebase SDK (Google, Apple, ...) and sends the ID token
as `Authorization: Bearer <token>`. Tokens last an hour and the SDK renews them,
so the API keeps no sessions. It checks the signature against Google's published
certificates, the audience and issuer for FIREBASE_PROJECT_ID, and then that the
email is verified and listed in AIMAP_ALLOWED_EMAILS. Any Google account can sign
in to a Firebase project, so the allowlist is what keeps strangers out.
"""

from __future__ import annotations

import re
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any, Protocol

from google.auth import exceptions as google_exceptions
from google.oauth2 import id_token


class AuthError(Exception):
    """The token is missing, invalid or expired: 401."""


class Forbidden(AuthError):
    """The token is valid but its email is not allowed: 403."""


@dataclass(frozen=True)
class User:
    uid: str
    email: str


class Verifier(Protocol):
    def verify(self, token: str) -> User: ...


def authorize(claims: dict[str, Any], project_id: str, allowed_emails: frozenset[str]) -> User:
    """Checks google-auth leaves to the caller. Pure, so it is tested without tokens."""
    if claims.get("iss") != f"https://securetoken.google.com/{project_id}":
        raise AuthError("token issued for another project")
    uid = claims.get("sub")
    if not uid:
        raise AuthError("token has no subject")
    email = (claims.get("email") or "").lower()
    if not email or claims.get("email_verified") is not True:
        raise Forbidden("email not verified")
    if email not in allowed_emails:
        raise Forbidden("email not allowed")
    return User(uid=uid, email=email)


_MAX_AGE = re.compile(r"max-age=(\d+)")


class CachingRequest:
    """google-auth transport that keeps GET responses for their Cache-Control max-age.

    Firebase's signing certificates change every few hours and say so in their
    headers. Without this every API call would fetch them again.
    """

    def __init__(self, inner: Callable[..., Any], clock: Callable[[], float] = time.monotonic,
                 default_ttl: float = 300.0) -> None:
        self.inner = inner
        self.clock = clock
        self.default_ttl = default_ttl
        self._cache: dict[str, tuple[float, Any]] = {}
        self._lock = threading.Lock()

    def __call__(self, url: str, method: str = "GET", **kwargs: Any) -> Any:
        if method != "GET":
            return self.inner(url, method=method, **kwargs)
        now = self.clock()
        with self._lock:
            hit = self._cache.get(url)
        if hit and hit[0] > now:
            return hit[1]
        response = self.inner(url, method=method, **kwargs)
        if response.status == 200:
            m = _MAX_AGE.search((response.headers or {}).get("cache-control", ""))
            ttl = float(m.group(1)) if m else self.default_ttl
            with self._lock:
                self._cache[url] = (now + ttl, response)
        return response


class FirebaseVerifier:
    def __init__(self, project_id: str, allowed_emails: frozenset[str], request: Callable[..., Any] | None = None,
                 clock_skew: int = 10) -> None:
        if request is None:
            from google.auth.transport.requests import Request

            request = CachingRequest(Request())
        self.project_id = project_id
        self.allowed_emails = allowed_emails
        self.request = request
        self.clock_skew = clock_skew

    def verify(self, token: str) -> User:
        try:
            claims = id_token.verify_firebase_token(token, self.request, audience=self.project_id,
                                                    clock_skew_in_seconds=self.clock_skew)
        except (ValueError, google_exceptions.GoogleAuthError) as e:
            raise AuthError(f"invalid token: {e}") from e
        return authorize(dict(claims), self.project_id, self.allowed_emails)
