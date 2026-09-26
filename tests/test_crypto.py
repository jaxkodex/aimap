import pytest

from aimap.crypto import Cipher, CryptoError, generate_key


def test_roundtrip_and_no_plaintext():
    c = Cipher(generate_key())
    token = c.encrypt("me@x.com", "s3cret")
    assert "s3cret" not in token and "me@x.com" not in token
    assert c.decrypt("me@x.com", token) == "s3cret"
    assert repr(c) == "Cipher(<redacted>)"


def test_token_bound_to_user():
    c = Cipher(generate_key())
    token = c.encrypt("a@x.com", "pw")
    with pytest.raises(CryptoError, match="different account"):
        c.decrypt("b@x.com", token)


def test_wrong_key_rejected():
    token = Cipher(generate_key()).encrypt("a", "pw")
    with pytest.raises(CryptoError, match="wrong key"):
        Cipher(generate_key()).decrypt("a", token)


def test_rotation():
    old, new = generate_key(), generate_key()
    token = Cipher(old).encrypt("a", "pw")
    both = Cipher(f"{new}, {old}")
    assert both.decrypt("a", token) == "pw"
    rotated = both.rotate(token)
    assert Cipher(new).decrypt("a", rotated) == "pw"
    with pytest.raises(CryptoError):
        Cipher(old).decrypt("a", rotated)


@pytest.mark.parametrize("key", ["", " , ", "not-a-key"])
def test_bad_keys(key):
    with pytest.raises(CryptoError):
        Cipher(key)
