# Approval review formatting

The client directly uses the existing `serde_json` dependency to render
complete tool arguments and proposed edit replacements in one ordered
review. Keeping this in the client avoids extension-specific formatting in
the protocol and copies in each renderer. A separate JSON parser or custom
parser would duplicate an existing maintained decoder.

Before and after promoting the client's existing dev dependency to a normal
dependency, `cargo tree -p client -e normal --prefix none -f '{p}'` has the
same set of graph entries: zero added or removed packages. `serde_json` was
already linked through the protocol. The review retains the original JSON
alongside literal multiline values, so decoding for display cannot lose
unknown arguments or change their original representation.

`mise run size` measured 3,398 KiB before and 3,414 KiB after the complete
change: 16 KiB added. Timings and resource budgets
are deferred to a quiet machine, per the shared-machine task instructions.
