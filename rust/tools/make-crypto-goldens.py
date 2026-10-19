"""Generate golden Fernet vectors with the real `cryptography` implementation.

Writes tests/fixtures/crypto_golden.json. The Rust port must decrypt these
tokens (and reject the bad ones) exactly like aimap.crypto.Cipher does.

Run:  python3 tools/make-crypto-goldens.py
"""

from __future__ import annotations

import base64
import json
from pathlib import Path

from cryptography.fernet import Fernet

FIXTURES = Path(__file__).resolve().parent.parent / "tests" / "fixtures"


def key() -> str:
    return Fernet.generate_key().decode()


def encrypt(cipher_key: str, user: str, password: str) -> str:
    return Fernet(cipher_key.encode()).encrypt(json.dumps({"user": user, "password": password}).encode()).decode()


def main() -> None:
    k1, k2, k3 = key(), key(), key()

    cases = [
        # (name, keys, token, user, expected password or expected error)
        {
            "name": "simple",
            "keys": [k1],
            "token": encrypt(k1, "me@x.com", "s3cret"),
            "user": "me@x.com",
            "expect_password": "s3cret",
        },
        {
            "name": "unicode",
            "keys": [k2],
            "token": encrypt(k2, "josé@x.com", "contraseña-ñ-🔐"),
            "user": "josé@x.com",
            "expect_password": "contraseña-ñ-🔐",
        },
        {
            "name": "empty_password",
            "keys": [k2],
            "token": encrypt(k2, "a@x.com", ""),
            "user": "a@x.com",
            "expect_password": "",
        },
        {
            "name": "rotation_secondary_key",
            "keys": [k2, k1],
            "token": encrypt(k1, "a@x.com", "legacy-pw"),
            "user": "a@x.com",
            "expect_password": "legacy-pw",
        },
        {
            "name": "rotation_third_key",
            "keys": [k3, k2, k1],
            "token": encrypt(k1, "a@x.com", "deep-legacy"),
            "user": "a@x.com",
            "expect_password": "deep-legacy",
        },
        {
            "name": "wrong_user_rejected",
            "keys": [k1],
            "token": encrypt(k1, "a@x.com", "pw"),
            "user": "b@x.com",
            "expect_error": "different account",
        },
        {
            "name": "wrong_key_rejected",
            "keys": [k2],
            "token": encrypt(k1, "a@x.com", "pw"),
            "user": "a@x.com",
            "expect_error": "wrong key",
        },
    ]

    # a corrupted token (valid b64, wrong bytes)
    bad = base64.urlsafe_b64encode(b"\x00" * 57).decode()
    cases.append({"name": "corrupted_token", "keys": [k1], "token": bad, "user": "a@x.com",
                  "expect_error": "wrong key"})

    out = {
        # for encryption parity: a known key so Rust ciphertexts can be verified outside
        "generate_key_sample": k3,
        "cases": cases,
    }
    FIXTURES.mkdir(parents=True, exist_ok=True)
    (FIXTURES / "crypto_golden.json").write_text(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {FIXTURES / 'crypto_golden.json'} with {len(cases)} cases")


if __name__ == "__main__":
    main()
