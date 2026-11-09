#!/usr/bin/env bash
# Tests for scripts/check_security_exceptions.py: good, expired, missing field.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
future=$(date -u -d '+30 days' +%F)
cat > "$tmp/good.json" <<JSON
[{"id":"X-1","tool":"trivy","reason":"r","owner":"Stanley Sujith Nelavala","expires":"$future","review_note":"n"}]
JSON
cat > "$tmp/expired.json" <<JSON
[{"id":"X-1","tool":"trivy","reason":"r","owner":"Stanley Sujith Nelavala","expires":"2020-01-01","review_note":"n"}]
JSON
cat > "$tmp/missing.json" <<JSON
[{"id":"X-1","tool":"trivy","owner":"Stanley Sujith Nelavala","expires":"$future","review_note":"n"}]
JSON
python3 "$here/check_security_exceptions.py" "$tmp/good.json" >/dev/null || { echo "good case failed"; exit 1; }
if python3 "$here/check_security_exceptions.py" "$tmp/expired.json" >/dev/null; then echo "expired case passed"; exit 1; fi
if python3 "$here/check_security_exceptions.py" "$tmp/missing.json" >/dev/null; then echo "missing case passed"; exit 1; fi
python3 "$here/check_security_exceptions.py" "$here/../security/exceptions.json" >/dev/null
echo "all exception checker cases behave as expected"
