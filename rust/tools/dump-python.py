"""Ground-truth twin of `rust/examples/dump.rs`.

Reads a raw RFC 822 message on stdin and prints parse_meta + body_text as
JSON, with exactly the same shape and key names as the Rust example, so the
two implementations can be diffed mechanically:

    python3 tools/dump-python.py < msg.eml   > py.json
    cargo run --quiet --example dump < msg.eml > rs.json
    diff py.json rs.json

Only stdlib is used (same as src/aimap/parse.py).
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))

from aimap.parse import body_text, parse_meta  # noqa: E402


def main() -> None:
    raw = sys.stdin.buffer.read()
    meta = parse_meta(raw)
    out = {
        "meta": {
            "rfc822_message_id": meta.rfc822_message_id,
            "from_email": meta.from_email,
            "from_name": meta.from_name,
            "subject": meta.subject,
            "sent_at": (
                meta.sent_at.isoformat().replace("+00:00", "Z") if meta.sent_at else None
            ),
            "in_reply_to": meta.in_reply_to,
            "references": meta.references,
            "has_list_headers": meta.has_list_headers,
        },
        "body_text": body_text(raw, 1500),
    }
    print(json.dumps(out, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
