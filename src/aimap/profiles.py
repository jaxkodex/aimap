"""Profiles (recipient context) and their patterns, stored in Postgres.

A profile is free-form JSON that Jev reads as `recipient_profile`, for example:

    {"who": "Alex, a freelance designer in Lisbon.",
     "active_priorities": ["Invoices from clients", "Renewing the studio lease"],
     "low_value": ["Retail coupons", "Social media digests"]}

Every account uses one profile, `default` unless changed with
`aimap accounts set-profile`.
"""

from __future__ import annotations

import csv
import io
import json
from dataclasses import dataclass

from psycopg import Connection
from psycopg.types.json import Jsonb

from aimap.jev import Pattern


class ProfileError(ValueError):
    pass


@dataclass(frozen=True)
class Profile:
    id: int
    name: str
    profile: dict


def get(conn: Connection, name: str) -> Profile:
    row = conn.execute("SELECT id, name, profile FROM profiles WHERE name = %s", (name,)).fetchone()
    if row is None:
        raise ProfileError(f"no profile {name!r}")
    return Profile(*row)


def list_all(conn: Connection) -> list[tuple[str, int, int]]:
    """(name, accounts, patterns) per profile."""
    return conn.execute("""
        SELECT p.name,
               (SELECT count(*) FROM accounts a WHERE a.profile_id = p.id),
               (SELECT count(*) FROM patterns t WHERE t.profile_id = p.id)
        FROM profiles p ORDER BY p.name""").fetchall()


def put(conn: Connection, name: str, profile: dict) -> None:
    if not isinstance(profile, dict):
        raise ProfileError("a profile must be a JSON object")
    conn.execute("""
        INSERT INTO profiles (name, profile) VALUES (%s, %s)
        ON CONFLICT (name) DO UPDATE SET profile = EXCLUDED.profile, updated_at = now()""",
                 (name, Jsonb(profile)))


def assign(conn: Connection, address: str, name: str) -> None:
    profile = get(conn, name)
    conn.execute("""
        INSERT INTO accounts (address, profile_id) VALUES (%s, %s)
        ON CONFLICT (address) DO UPDATE SET profile_id = EXCLUDED.profile_id""", (address, profile.id))


def account_profiles(conn: Connection) -> list[tuple[str, str]]:
    return conn.execute("""
        SELECT a.address, p.name FROM accounts a JOIN profiles p ON p.id = a.profile_id
        ORDER BY a.address""").fetchall()


def patterns(conn: Connection, profile_id: int) -> list[Pattern]:
    rows = conn.execute("""
        SELECT insight, importance, action_bucket, tags, examples FROM patterns
        WHERE profile_id = %s ORDER BY insight""", (profile_id,)).fetchall()
    return [Pattern(insight=r[0], importance=r[1], action_bucket=r[2], tags=list(r[3]), examples=r[4])
            for r in rows]


def replace_patterns(conn: Connection, profile_id: int, items: list[Pattern]) -> None:
    """Replace every pattern of the profile. Changes the classifier key, so new mail uses the new set."""
    seen = set()
    for p in items:
        p.validate()
        if p.insight in seen:
            raise ProfileError(f"duplicate pattern {p.insight!r}")
        seen.add(p.insight)
    conn.execute("DELETE FROM patterns WHERE profile_id = %s", (profile_id,))
    for p in items:
        conn.execute("""
            INSERT INTO patterns (profile_id, insight, importance, action_bucket, tags, examples)
            VALUES (%s, %s, %s, %s, %s, %s)""",
                     (profile_id, p.insight, p.importance, p.action_bucket, p.tags, Jsonb(p.examples)))


def parse_patterns(text: str, fmt: str) -> list[Pattern]:
    """JSON: a list of {insight, importance, action_bucket, tags, examples}.
    CSV: columns insight, importance, action_bucket, tags (';'-separated), and optional
    example_from / example_subject (rows with the same insight add examples)."""
    if fmt == "json":
        data = json.loads(text)
        if not isinstance(data, list):
            raise ProfileError("patterns JSON must be a list")
        try:
            return [Pattern(insight=d["insight"], importance=d["importance"], action_bucket=d["action_bucket"],
                            tags=list(d.get("tags", [])), examples=list(d.get("examples", []))) for d in data]
        except (KeyError, TypeError) as e:
            raise ProfileError(f"invalid pattern: {e}") from e
    if fmt == "csv":
        merged: dict[str, dict] = {}
        for row in csv.DictReader(io.StringIO(text)):
            try:
                insight = row["insight"].strip()
                p = merged.setdefault(insight, {
                    "importance": row["importance"].strip(), "action_bucket": row["action_bucket"].strip(),
                    "tags": [t.strip() for t in (row.get("tags") or "").split(";") if t.strip()],
                    "examples": [],
                })
            except (KeyError, AttributeError) as e:
                raise ProfileError(f"CSV needs insight, importance, action_bucket columns ({e})") from e
            if row.get("example_subject") and len(p["examples"]) < 3:
                p["examples"].append({"from": (row.get("example_from") or "").strip(),
                                      "subject": row["example_subject"].strip()})
        return [Pattern(insight=k, **v) for k, v in merged.items()]
    raise ProfileError(f"unknown patterns format {fmt!r}")
