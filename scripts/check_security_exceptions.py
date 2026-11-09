#!/usr/bin/env python3
"""Check security/exceptions.json. Exit 1 when an item is incomplete or expired."""
import datetime
import json
import sys

REQUIRED = ("id", "tool", "reason", "owner", "expires", "review_note")


def check(path: str, today: datetime.date | None = None) -> list[str]:
    today = today or datetime.date.today()
    errors: list[str] = []
    with open(path, encoding="utf-8") as handle:
        items = json.load(handle)
    if not isinstance(items, list):
        return ["file must hold a JSON list"]
    for index, item in enumerate(items):
        for field in REQUIRED:
            if not item.get(field):
                errors.append(f"item {index}: missing field {field}")
        expires = item.get("expires")
        if expires:
            try:
                date = datetime.date.fromisoformat(expires)
            except ValueError:
                errors.append(f"item {index}: expires is not an ISO date")
                continue
            if date < today:
                errors.append(f"item {index}: expired on {expires}")
            elif date > today + datetime.timedelta(days=90):
                errors.append(f"item {index}: expires more than 90 days ahead")
    return errors


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "security/exceptions.json"
    errors = check(path)
    for error in errors:
        print(f"FAIL {error}")
    if not errors:
        print("OK exceptions file is valid")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
