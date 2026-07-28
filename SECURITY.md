# Security and data-safety policy

Report vulnerabilities privately to the repository owner. Include the version,
OS build, exact command, log/report with sensitive paths redacted, and whether
any destination final name or unrelated object was affected.

Data-loss, source-write, arbitrary-delete, path-escape, temp-ownership,
replacement-race, audit-integrity, and resume-prefix issues are critical. Stop
using the affected build and preserve artifacts. Do not reproduce against user
data or removable media; use the validated C: sandbox process in
`docs/TESTING.md`.
