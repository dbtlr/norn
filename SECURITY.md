# Security Policy

Thanks for taking the time to disclose a security issue responsibly.

## Reporting a vulnerability

Email security reports to **hi@dbtlr.com**. Please do not file a public GitHub issue.

Include:

- The version of `norn` affected (`norn --version`).
- The platform you reproduced on.
- A description of the issue, ideally with a minimal reproducer.
- Whether the issue has been disclosed anywhere else.

## Response expectations

- **Acknowledgement:** within 48 hours of receipt.
- **Initial assessment:** within 7 days.
- **Fix or mitigation timeline:** communicated once the assessment is complete. Critical issues are prioritized; lower-severity issues are scheduled into the next reasonable release.
- **Public disclosure:** coordinated with the reporter. The default is to publish a security advisory and CHANGELOG entry once a fix is available.

There is no bug bounty program. Credit for reports is offered in the security advisory and CHANGELOG unless the reporter prefers to remain anonymous.

## Supported versions

Only the latest minor release receives security fixes. `norn` is pre-1.0; minor releases may include breaking changes, and users are encouraged to stay close to the latest release. Backports to older minors are not guaranteed.

| Version          | Supported |
|------------------|-----------|
| Latest minor     | Yes       |
| Older minors     | No        |

## Scope

In scope:

- The `norn` CLI binary (single-crate since v0.34; formerly composed of `vault-core`, `vault-frontmatter`, `vault-links`, `vault-graph`, `vault-standards`).
- The published shell installer and binary release artifacts.

Out of scope:

- Vulnerabilities in dependencies that are already publicly tracked (file those upstream; we will pick up the fix on the next release).
- Issues that require an attacker who already has write access to a user's filesystem or shell.
