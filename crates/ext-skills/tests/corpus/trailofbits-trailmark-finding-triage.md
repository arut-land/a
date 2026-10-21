---
name: trailmark-finding-triage
description: "Performs graph-assisted triage of a single security finding, SARIF result, weAudit annotation, suspicious function, or report excerpt using Trailmark reachability, entrypoint paths, taint, privilege-boundary, blast-radius, caller/callee, and neighborhood evidence. Use when deciding whether one candidate issue is reachable, prioritizing a finding before PoC work, preparing evidence for exploit validation, or checking whether a static-analysis result is actionable."
allowed-tools:
  - Bash
  - Read
  - Grep
  - Glob
  - Write
---
