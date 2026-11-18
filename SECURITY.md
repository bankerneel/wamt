# Security Policy

## Supported versions

Only the latest released version receives security fixes.

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/bankerneel/wamt/security/advisories/new) for this repository. Do not open a public issue for a security problem.

Include the version, the platform, a short description, reproduction steps and the impact you see. You should receive an acknowledgement within a few days. We will work with you on a fix and a coordinated disclosure.

## Scope

wamt reads local T3 Code, Claude Code and Codex data and serves a dashboard on loopback. Reports about the following are especially welcome:

- Any path that writes to or locks a source directory
- Exposure of message text or other content beyond metadata
- Ways for a remote page to read the dashboard (Host check, CSP, DNS rebinding)
- Path traversal in asset serving or unsafe handling of query parameters
