# Security policy

Please report suspected vulnerabilities through GitHub's private vulnerability
reporting or Security Advisory interface when available. Avoid filing a public
issue that includes credentials, private network details, or an exploitable
proof of concept.

Runtime configuration is not part of the source repository. If a credential is
accidentally committed, revoke or rotate it immediately; removing it in a later
commit is not sufficient because it remains in Git history.
