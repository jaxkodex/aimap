"""Encryption for stored credentials.

Uses Fernet (AES-128-CBC + HMAC-SHA256) from `cryptography`. AIMAP_SECRET_KEY
holds one or more comma-separated keys. The first encrypts; all of them can
decrypt, so a key can be rotated without downtime:

    1. prepend a new key:     AIMAP_SECRET_KEY=<new>,<old>
    2. re-encrypt the file:   aimap accounts rotate-key
    3. drop the old key:      AIMAP_SECRET_KEY=<new>

Each token encrypts {"user", "password"} and decryption checks the user, so a
ciphertext copied onto another account's record is rejected.
"""

from __future__ import annotations

import json

from cryptography.fernet import Fernet, InvalidToken, MultiFernet


class CryptoError(ValueError):
    pass


def generate_key() -> str:
    return Fernet.generate_key().decode()


class Cipher:
    def __init__(self, keys: str | list[str]) -> None:
        if isinstance(keys, str):
            keys = [k.strip() for k in keys.split(",")]
        keys = [k for k in keys if k]
        if not keys:
            raise CryptoError("AIMAP_SECRET_KEY is empty")
        try:
            self._f = MultiFernet([Fernet(k.encode()) for k in keys])
        except ValueError as e:
            raise CryptoError("AIMAP_SECRET_KEY must be Fernet keys (run `aimap keygen`)") from e

    def __repr__(self) -> str:
        return "Cipher(<redacted>)"

    def encrypt(self, user: str, password: str) -> str:
        blob = json.dumps({"user": user, "password": password}).encode()
        return self._f.encrypt(blob).decode()

    def decrypt(self, user: str, token: str) -> str:
        try:
            data = json.loads(self._f.decrypt(token.encode()))
        except (InvalidToken, ValueError) as e:
            raise CryptoError(f"cannot decrypt credentials for {user}: wrong key or corrupted token") from e
        if data.get("user") != user:
            raise CryptoError(f"credentials for {user} were encrypted for a different account")
        return data["password"]

    def rotate(self, token: str) -> str:
        """Re-encrypt with the primary key."""
        try:
            return self._f.rotate(token.encode()).decode()
        except InvalidToken as e:
            raise CryptoError("cannot rotate token: no configured key decrypts it") from e
