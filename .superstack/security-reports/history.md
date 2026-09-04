# Security report history

## 2026-09-04 — mintbot
- Mode: Whole-code review remediation verification
- Review issues addressed: 7; corrected: 6; mitigated with inclusion-time fee caveat: 1
- Verification: 67 tests pass; formatting, Clippy, and redacted secret scans pass
- Dependency audit: no known vulnerability advisories; 2 upstream maintenance warnings remain
- Withdrawn dependency updated: chacha20 0.10.1 → 0.10.2

## 2026-08-22 — mintbot
- Mode: Daily full-phase audit
- Findings: 12 (C: 0, H: 1, M: 7, L: 4, I: 0)
- New: 12 | Resolved from prior reports: 0 | Persistent from prior reports: 0
- Remediated during audit: 10 | Partially resolved: 2
- Confidence gate: 8/10

## 2026-09-03 — mintbot
- Mode: Daily diff audit (`added hyperevm`)
- Findings: 0 (C: 0, H: 0, M: 0, L: 0, I: 0)
- New: 0 | Resolved: 0 | Persistent in diff scope: 0
- Confidence gate: 8/10
