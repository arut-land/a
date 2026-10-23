use std::fmt::Write as _;
use std::io;
use std::path::Path;

pub trait Dialect: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const PROMPT: &'static str;
    fn arguments(command: &str, dir: &Path, report: Option<&Path>) -> io::Result<Vec<String>>;
}

pub struct Bash;
pub struct PowerShell;

impl Dialect for Bash {
    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "Runs Bash syntax in the session's working directory. Each call starts a fresh Bash without profile or rc files; cwd persists, variables do not. Returns combined stdout/stderr and the process exit status. No stdin or terminal.";
    const PROMPT: &'static str = "$ ";

    fn arguments(command: &str, dir: &Path, report: Option<&Path>) -> io::Result<Vec<String>> {
        let pwd_command = sys::shell_pwd();
        if pwd_command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native Bash runtime is unsupported: expected MSYS/Git Bash or Cygwin; PowerShell is selected only when Bash is absent",
            ));
        }
        let mut script = String::new();
        if let Some(report) = report {
            let pwd = format!("{} >{}", pwd_command, bash_quote(&sys::shell_path(report)));
            let _ = writeln!(script, "trap {} EXIT", bash_quote(&pwd));
        }
        let _ = writeln!(
            script,
            "cd -- {} || exit\n{command}",
            bash_quote(&sys::shell_path(dir))
        );
        Ok(["--noprofile", "--norc", "-c", &script]
            .map(str::to_owned)
            .into())
    }
}

fn bash_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn ps_quote(path: &Path) -> String {
    format!("'{}'", sys::shell_path(path).replace('\'', "''"))
}

impl Dialect for PowerShell {
    const NAME: &'static str = "powershell";
    const DESCRIPTION: &'static str = "Runs PowerShell syntax in a fresh noninteractive shell without profiles. Filesystem cwd persists, variables do not. Use Set-Location -LiteralPath and single-quoted literal paths (double embedded apostrophes); environment variables use $env:NAME. Explicit exit N preserves N. Otherwise the final command's $? yields exit 0 or 1; terminating errors yield 1. A native failure followed by success yields 0 even if $LASTEXITCODE is stale; use exit $LASTEXITCODE immediately to preserve a native exit code. Non-terminating errors keep normal PowerShell semantics. Returns combined plain-text output, decoded as UTF-8 (arbitrary native output may use another encoding). No stdin or terminal. Complete wrapper plus command must fit 30,000 UTF-16 code units; oversized input is rejected.";
    const PROMPT: &'static str = "PS> ";

    fn arguments(command: &str, dir: &Path, report: Option<&Path>) -> io::Result<Vec<String>> {
        let mut script = format!(
            "$OutputEncoding = [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
             $ProgressPreference = 'SilentlyContinue'\n\
             if (Get-Variable PSStyle -ErrorAction Ignore) {{ $PSStyle.OutputRendering = 'PlainText' }}\n\
             Set-Location -LiteralPath {} -ErrorAction Stop\n\
             $__a_ok = $true\ntry {{\n{command}\n$__a_ok = $?\n}} catch {{\n\
             $__a_ok = $false\n[Console]::Error.WriteLine($_.ToString())\n}} finally {{\n",
            ps_quote(dir),
        );
        if let Some(report) = report {
            // FileSystem paths only: Registry/provider locations cannot become
            // the next process's working directory. Explicit exit still runs finally.
            let _ = writeln!(
                script,
                "if ($PWD.Provider.Name -eq 'FileSystem') {{ [System.IO.File]::WriteAllText({}, $PWD.ProviderPath, [System.Text.UTF8Encoding]::new($false)) }}",
                ps_quote(report)
            );
        }
        script.push_str("}\nif (-not $__a_ok) { exit 1 }\n");
        // Native argv is Unicode and SpawnPlan quotes it. -Command keeps 5.1
        // stderr textual; -EncodedCommand implicitly selects CLIXML stderr.
        if script.encode_utf16().count() > 30_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PowerShell command plus wrapper exceeds 30,000 UTF-16 code units",
            ));
        }
        Ok([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-OutputFormat",
            "Text",
            "-Command",
            &script,
        ]
        .map(str::to_owned)
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_literal_paths_and_command_limit_are_explicit() {
        let args =
            PowerShell::arguments("Write-Output 'ok'", Path::new("space ' [café]"), None).unwrap();
        assert!(
            args.last()
                .unwrap()
                .contains("-LiteralPath 'space '' [café]'")
        );
        assert!(!args.iter().any(|arg| arg.contains("Bypass")));
        assert!(PowerShell::arguments(&"x".repeat(30_000), Path::new("."), None).is_err());
    }
}
