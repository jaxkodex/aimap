import json
import time
from datetime import UTC, datetime, timedelta
from types import SimpleNamespace

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID
from google.auth import crypt, jwt

from aimap.auth import AuthError, CachingRequest, FirebaseVerifier, Forbidden, authorize

PROJECT = "aimap-test"
ALLOWED = frozenset({"me@example.com"})


def _keypair():
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "test")])
    now = datetime.now(UTC)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
            .serial_number(1).not_valid_before(now - timedelta(days=1)).not_valid_after(now + timedelta(days=1))
            .sign(key, hashes.SHA256()))
    pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                            serialization.NoEncryption()).decode()
    return pem, cert.public_bytes(serialization.Encoding.PEM).decode()


PRIVATE, CERT = _keypair()


class FakeCerts:
    """google-auth transport serving one certificate, counting fetches."""

    def __init__(self, certs=None, max_age=3600):
        self.certs = certs if certs is not None else {"k1": CERT}
        self.max_age = max_age
        self.calls = 0

    def __call__(self, url, method="GET", **kwargs):
        self.calls += 1
        return SimpleNamespace(status=200, data=json.dumps(self.certs).encode(),
                               headers={"cache-control": f"public, max-age={self.max_age}"})


def token(**overrides):
    now = int(time.time())
    claims = {"iss": f"https://securetoken.google.com/{PROJECT}", "aud": PROJECT, "sub": "uid-1",
              "email": "Me@Example.com", "email_verified": True, "iat": now, "exp": now + 3600,
              "firebase": {"sign_in_provider": "google.com"}, **overrides}
    return jwt.encode(crypt.RSASigner.from_string(PRIVATE, key_id="k1"), claims).decode()


def verifier(request=None):
    return FirebaseVerifier(PROJECT, ALLOWED, request=request or FakeCerts())


def test_valid_token_returns_user_with_lowercased_email():
    user = verifier().verify(token())
    assert (user.uid, user.email) == ("uid-1", "me@example.com")


@pytest.mark.parametrize("overrides", [
    {"aud": "other-project"},
    {"iss": "https://securetoken.google.com/other-project"},
    {"exp": int(time.time()) - 3600, "iat": int(time.time()) - 7200},
    {"sub": ""},
])
def test_rejects_token_for_another_project_or_expired(overrides):
    with pytest.raises(AuthError) as e:
        verifier().verify(token(**overrides))
    assert not isinstance(e.value, Forbidden)


def test_rejects_token_signed_with_another_key():
    other_private, _ = _keypair()
    now = int(time.time())
    bad = jwt.encode(crypt.RSASigner.from_string(other_private, key_id="k1"),
                     {"iss": f"https://securetoken.google.com/{PROJECT}", "aud": PROJECT, "sub": "u",
                      "email": "me@example.com", "email_verified": True, "iat": now, "exp": now + 60}).decode()
    with pytest.raises(AuthError):
        verifier().verify(bad)


def test_rejects_garbage():
    with pytest.raises(AuthError):
        verifier().verify("not-a-jwt")


@pytest.mark.parametrize("overrides", [{"email": "someone@example.com"}, {"email_verified": False}, {"email": None}])
def test_valid_token_but_not_allowed_is_forbidden(overrides):
    with pytest.raises(Forbidden):
        verifier().verify(token(**overrides))


def test_authorize_is_fail_closed_with_empty_allowlist():
    claims = {"iss": f"https://securetoken.google.com/{PROJECT}", "sub": "u", "email": "me@example.com",
              "email_verified": True}
    with pytest.raises(Forbidden):
        authorize(claims, PROJECT, frozenset())


def test_certificates_are_cached_for_their_max_age():
    certs = FakeCerts(max_age=100)
    clock = [0.0]
    v = verifier(CachingRequest(certs, clock=lambda: clock[0]))
    v.verify(token())
    v.verify(token())
    assert certs.calls == 1
    clock[0] = 101.0
    v.verify(token())
    assert certs.calls == 2
