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
output is sealed into a blob the result references, and background output stays
an open blob stream while the job runs, sealed as its group ends.

## Timeout moves a command to the background

A command's timeout (`[ext.shell] timeout`, 120 s by default; per call at most
600 s) is how long the session waits on it, not how long it may run. Past it,
the shell sets the call's `Cx::detach`, the flag a forced steer sets: the call
is answered with a placeholder naming its output stream, and its result comes
back on the queue once the command ends, holding its whole output as a blob
when the model saw it cut. There is no hard upper bound on a backgrounded
command: like a `background: true` job it is session-owned work, ended by an
interrupt, its session's end or the 64 MiB output cap. Only a call another
call invoked (a Lua script's) is stopped at its timeout, since its caller
must wait for it. After the timeout passes the supervisor arms no timer: it
waits only on output, the shell's exit and cancellation.

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

## Native Desktop standard library

On the Windows 11 Arm runner, implicit 5.1 discovery stalled even for
`Write-Output`, `Get-Variable` and `Set-Location`. Clearing the inherited module
path before process startup did not resolve it: Desktop startup reconstructs
that path and can insert AllUsers ahead of its own modules. A
[bounded native probe](https://github.com/arut-land/a/actions/runs/39998112920/job/120567421864)
completed with exit 37 after either replacing the module path inside the
running shell or importing the native Utility manifest by full path. Disabling
the analysis cache did not fix discovery. Separate probes ruled out process
machine choice and console flags.

The Desktop SDK tool therefore imports Utility and Management explicitly from
its own `$PSHOME` before executing commands. It preserves the client's module
path and normal error preferences. The pwsh tool uses its own initialization.
The discovered edition selects these concrete SDK types at startup; both
publish the single name `powershell`. The process service and SpawnPlan retain
their existing launch and supervision contracts.

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

## Hosted observations

The [paired run](https://github.com/arut-land/a/actions/runs/40001426239)
compares production revision `17a7b27d` with main `781586d5`, with five
alternating rounds per build on each native runner. The subsequent `c730c03e`
changes only the PowerShell test deadline from 20 to 90 seconds; production
code is identical. The [functional run](https://github.com/arut-land/a/actions/runs/40006363910)
uses that test deadline with no diagnostic instrumentation.

Release sizes, in KiB:

| Native target | Main | Candidate | Difference |
| --- | ---: | ---: | ---: |
| Linux x86_64 | 4160.656 | 4192.070 | +31.414 |
| Linux ARM64 | 3582.914 | 3604.539 | +21.625 |
| Windows x86_64 | 5340.000 | 5405.000 | +65.000 |
| Windows ARM64 | 4698.500 | 4754.500 | +56.000 |
| macOS x86_64 | 3864.828 | 3896.930 | +32.102 |
| macOS ARM64 | 3219.625 | 3236.031 | +16.406 |

Paired median differences (candidate minus main):

| Native target | First reply, ms | Attach p50, ms | Attach p99, ms | Startup, ms | Idle RSS, KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | -0.111 | -0.014 | -0.005 | +0.017 | +16 |
| Linux ARM64 | -0.395 | +0.003 | -0.001 | -0.022 | +28 |
| Windows x86_64 | -0.422 | -0.007 | +0.014 | -0.109 | +56 |
| Windows ARM64 | -1.478 | +0.004 | +0.029 | +0.510 | +96 |
| macOS x86_64 | +10.035 | +0.104 | +0.056 | +0.417 | +40 |
| macOS ARM64 | +4.912 | -0.063 | -0.027 | -0.885 | +48 |

These small samples are observations, not significance tests. First-reply
and startup median differences on these targets are smaller than the larger
of the two p10–p90 spreads. Linux x86_64 attach p50 improved by 0.014 ms and Windows
ARM64 attach p99 increased by 0.029 ms, exceeding both builds' observed spreads.
Idle RSS increases on Linux ARM64, Windows ARM64 and Intel macOS exceed those
spreads; the other RSS changes do not.

Daemon and line-client idle CPU medians are zero for both builds on these
targets. All Linux and macOS samples and all line-client samples are
zero. Windows x86_64 has one candidate daemon sample at 15.625 ms; Windows
ARM64 has nonzero daemon samples in both builds at that resolution. RSS is
sampled three seconds after first reply; CPU uses a ten-second window after
fifteen seconds of settling. A zero counter increment describes the OS
measurement resolution, not proof that no instructions executed.
