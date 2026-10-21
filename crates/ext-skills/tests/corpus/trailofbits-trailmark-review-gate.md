---
name: trailmark-review-gate
description: "Runs a Trailmark structural review gate over a branch, pull request, fix commit, release diff, or git ref range to detect new entrypoints, new tainted paths, removed validation or authorization calls, privilege-boundary drift, blast-radius growth, complexity growth, and newly reachable sensitive sinks. Use when reviewing a PR, branch, remediation commit, or release diff where graph-level security regressions should be checked before merge."
allowed-tools:
  - Bash
  - Read
  - Grep
  - Glob
  - Write
---
