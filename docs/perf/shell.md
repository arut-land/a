# Shell execution

The shell extension chooses one tool at daemon startup. Unix uses Bash;
Windows searches captured PATH, Git for Windows user/machine installation
registry entries, conventional Git installation directories and MSYS2 roots.
The system WSL Bash launcher is excluded. Supported Windows Bash adapters are
MSYS/Git and Cygwin; an unknown native runtime fails explicitly instead of
silently changing dialect. Without Bash, pwsh is selected before Windows
PowerShell 5.1. Later client environments do not rename the tool or replace
its executable. Restarting the daemon rediscovers installations.

Tool metadata and prompt context share immutable execution facts. Shell child
environments retain client PATH priority, append the selected Bash runtime's
utility directory, suppress Bash startup hooks and pin the published temporary
directory. Unix process-group cleanup permits descendants to escape their group;
Windows cleanup owns a kill-on-close job. Command-created temporary files remain
the command's responsibility. Foreground cwd receipts are best effort; truncated
and background output remains in the session spill directory.

## Windows executable names

The [native ARM64 startup probe](https://github.com/arut-land/a/actions/runs/39982341912/job/120512577646)
ran PowerShell 7.6.7 and Windows PowerShell 5.1.26100.9711 on Windows 11 Arm.
Each received `exit 37`, null stdin and redirected output, with both ordinary
and extended-length working-directory names:

| Executable spelling | PowerShell 7 | Windows PowerShell 5.1 |
| --- | ---: | ---: |
| Ordinary drive path | 37 | 37 |
| Extended-length prefix | 37 | 4294901760; initialization failure |

The 5.1 failure precedes the supplied command: the type initializer for
`System.Net.ServicePointManager` throws. Working-directory spelling does not
change this result. The executable resolver therefore uses `dunce` 1.0.5 on
Windows to return ordinary names when equivalent, retaining verbatim-only
names when simplification would change their meaning. This dependency has no
transitive dependencies. Standard canonicalization fails the measured startup
case; unconditional prefix removal would discard Windows path semantics.
Internal filesystem paths retain their native representation.

## Shell contract validation

Hosted Windows x64 and ARM64 CI runs the same SDK tool against both installed
PowerShell editions. The invariant covers explicit and final native exits,
stale LASTEXITCODE, terminating and non-terminating errors, handled errors,
empty commands, return, no-input behavior, Unicode and literal cwd changes,
parse failures and failed cwd receipts. Separate invariants cover tool
cardinality, selected executable descriptions, prompt facts, WSL exclusion,
PowerShell absence, environment casing and command-line quoting expansion.

The runner takes one Unicode `-Command` payload, with ordinary text host
streams rather than EncodedCommand's CLIXML stderr. It changes no execution
policy. The supplied body remains directly in try/finally so its final `$?`
is captured before receipt commands can replace it. Explicit exits retain
their numeric status; otherwise success/failure becomes 0/1. Arbitrary native
output remains bytes and is displayed with UTF-8 replacement.

The perf workflow reports release size on six native targets and compares
five alternating candidate/reference rounds against main. These observations
cover daemon startup, first reply, attach, RSS and idle CPU; they do not
measure PowerShell child startup or child working set.
