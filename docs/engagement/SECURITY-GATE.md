# Security gate

The workflow is `.github/workflows/security.yml`.

## Jobs and fail rules

| Job | What it checks | Fails when |
|---|---|---|
| `codeql` | CodeQL static analysis for Rust | Any error-level result. |
| `secrets` | Gitleaks over the full Git history with `.gitleaks.toml` | Any finding. |
| `deps` | `cargo audit` through `rustsec/audit-check` | A known vulnerability in `Cargo.lock`. |
| `image` | Build the image, then Trivy image scan | HIGH or CRITICAL finding with a fix. |
| `sbom` | CycloneDX bill of materials from Trivy | The scan fails. |
| `exceptions` | `scripts/check_security_exceptions.py` on `security/exceptions.json` | An item lacks a field or has expired. |

A finding in `security/exceptions.json` that has not expired does not fail the job. The list is empty now.

## Run each check locally

```
gitleaks git --config .gitleaks.toml --redact --no-banner --exit-code 1 .
gitleaks dir --config .gitleaks.toml --redact --no-banner .
cargo audit
trivy fs --scanners vuln --severity HIGH,CRITICAL --ignore-unfixed --exit-code 1 --skip-dirs target .
trivy image --severity HIGH,CRITICAL --ignore-unfixed --exit-code 1 localhost/webhook-connector-gateway:sec
trivy fs --format cyclonedx --output docs/engagement/evidence/sbom-source.cdx.json --skip-dirs target .
actionlint .github/workflows/security.yml
python3 scripts/check_security_exceptions.py security/exceptions.json
scripts/test_check_security_exceptions.sh
```

## First scan result

Gitleaks flagged the demo tenant key `flaky-api-key` in `scripts/demo.sh`. It is a made-up value for a local mock. It is now in the allowlist of `.gitleaks.toml` with a comment. No other finding.

## Status

| Control | Status |
|---|---|
| Local runs of the checks above | Verified locally (see `evidence/security-gate-local.txt`, measured on one laptop) |
| Workflow file lint | Verified locally with actionlint and yamllint |
| Workflow run on GitHub | Authored, not run: no Actions run was started or checked for this work |
| CodeQL job | Authored, not run: it needs GitHub Actions |
| Action versions | Unverified until a run |
