# Security policy

## Supported versions

Until the first stable release, security fixes are applied to the latest
release and the `main` branch. After 1.0, this table will list maintained
release lines explicitly.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting feature for this
repository: **Security → Advisories → Report a vulnerability**. If that
feature is unavailable, contact the maintainer through a private channel shown
on the repository owner's profile and ask for a secure reporting address.

Do not include an API token, private key, active configuration, or sensitive
logs in an issue, pull request, or unencrypted email. Revoke any token that may
have been exposed.

Include the affected version or commit, operating system, impact, reproduction
steps, and any proposed mitigation. You should receive an acknowledgement
within seven days. We will coordinate validation, remediation, disclosure, and
credit with you. Please allow a reasonable remediation window before public
disclosure.

## Operational security

Use a Cloudflare API token restricted to only the required zones with `Zone
Read` and `DNS Edit` permissions. Cirrusync does not need a Global API Key.
Keep `/etc/cirrusync/token` owned by `root:cirrusync` with mode `0640`, or use
an equivalently restrictive arrangement that still permits the unprivileged
service account to read it. Do not add access ACLs or other users to the
service group, and do not place access or default ACLs on
`/etc/cirrusync`.
