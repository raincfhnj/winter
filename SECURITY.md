# Security Policy

## Supported versions

Winter is pre-1.0. Security fixes are applied to the latest release on the
`master` branch.

| Version | Supported |
|---|---|
| 0.2.x | Yes |
| < 0.2 | No |

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Report privately through GitHub's
[private vulnerability reporting](https://github.com/raincfhnj/winter/security/advisories/new)
(Security tab → Report a vulnerability). Include:

- A description of the issue and its impact.
- Steps to reproduce, or a proof of concept.
- Affected version and Windows/Windows Terminal versions.
- Whether the controller was running elevated.

You can expect an acknowledgement within a few days. Please give us a reasonable window
to ship a fix before public disclosure.

## Threat model and scope

Winter installs a global low-level keyboard and mouse hook and runs elevated, so
the following are in scope:

- Unintended interception, logging, or transmission of keystrokes or pointer data.
- Injection of input into a window other than the captured foreground Windows Terminal.
- Bypass of the foreground identity / revalidation checks.
- Unsafe or privilege-escalating behavior in the elevation path.
- Corruption of, or unauthorized writes to, Windows Terminal configuration or user
  profile files.
- Path traversal, symlink following, or non-atomic writes in the integration layer.

The following are **not** treated as vulnerabilities by themselves:

- The controller must run elevated to control an elevated Windows Terminal; this is
  documented and intentional.
- Physical access to an already-compromised elevated desktop.
- Windows Terminal or Shell behavior outside Winter's control.

## Privacy

Winter does not collect telemetry, open network ports, or store terminal content
or keystroke history. If you find any code that contradicts this, please report it.
