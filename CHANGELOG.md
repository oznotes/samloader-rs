# Changelog

## 2.0.2

This release hardens samloader for public desktop distribution.

- Adds an all-known-build firmware explorer (stable, previous, and beta), CSC
  suggestions with custom-code support, connected-device model/CSC autofill,
  smarter download preflight and controls, and a reorganized UI that keeps the
  equivalent PowerShell CLI command visible and copyable.
- Fails closed for ambiguous devices, incompatible firmware entries, duplicate
  partitions, and incomplete repartition requests.
- Protects active downloads and flashes from conflicting operations, duplicate
  app instances, and accidental window closure.
- Uses authenticated firmware transport, verifies completed firmware archives,
  and installs downloads atomically through exclusive staging files.
- Makes device waiting cancellable in the desktop workflow and fixes the CLI
  wait behavior.
- Adds destructive CSC warnings, PowerShell-safe CLI command copying, resilient
  preferences, stale-state cleanup, and accessibility improvements.
- Adds debug and release test gates, dependency audits, signed Windows release
  automation, checksums, security policy, and third-party notices.
