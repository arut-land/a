//! Shell completions and the man page, written from the spec. The
//! completion scripts are usage's: each asks `a __complete_word__`, which
//! answers from the same tables the parser reads. usage writes no man page,
//! so this does. Only the tests link this: `mise run generate:completions`
//! and `generate:man` run the test below with `A_GENERATE` set to the
//! directory they go to.

use std::fmt::Write;
use std::path::Path;
use std::process::Command;

use usage_rs::complete::Shell;
use usage_rs::help::usage_line;
use usage_rs::spec::{CommandMeta, FlagMeta};

use super::Args;
use super::tests::commands;

/// Every shell usage writes a script for: the file it goes to, and the
/// command that checks it parses, where one is known.
const SHELLS: [(Shell, &str, Option<[&str; 2]>); 6] = [
    (Shell::Bash, "completions/a.bash", Some(["bash", "-n"])),
    (Shell::Zsh, "completions/a.zsh", Some(["zsh", "-n"])),
    (
        Shell::Fish,
        "completions/a.fish",
        Some(["fish", "--no-execute"]),
    ),
    (Shell::Nu, "completions/a.nu", None),
    (Shell::PowerShell, "completions/a.ps1", None),
    (Shell::Elvish, "completions/a.elv", None),
];

/// `text` for roff: backslashes and dashes escaped, and no line read as a
/// request.
fn roff(text: &str) -> String {
    let text = text.replace('\\', r"\e").replace('-', r"\-");
    if text.starts_with(['.', '\'']) {
        format!(r"\&{text}")
    } else {
        text
    }
}

/// The usage line, its words set in bold and its values in italics.
fn synopsis(path: &[&CommandMeta]) -> String {
    let names: Vec<_> = path.iter().map(|cmd| cmd.cmd().name).collect();
    let line = usage_line(&names, path[path.len() - 1]);
    let mut out = String::new();
    for word in line.split(' ') {
        let open = word.trim_start_matches('[');
        let bare = open.trim_end_matches(']');
        let (pre, post) = (&word[..word.len() - open.len()], &open[bare.len()..]);
        let set = match bare.strip_prefix('<').and_then(|v| v.strip_suffix('>')) {
            Some(value) => format!(r"\fI{}\fR", roff(value)),
            None if bare.chars().all(|c| c.is_ascii_uppercase()) => format!(r"\fI{bare}\fR"),
            None => format!(r"\fB{}\fR", roff(bare)),
        };
        _ = write!(out, "{pre}{set}{post} ");
    }
    out.trim_end().to_owned()
}

/// A flag's entry: its spellings, its value, its help and its variable.
fn flag(out: &mut String, flag: &FlagMeta) {
    let f = flag.flag();
    let mut names: Vec<_> = f
        .shorts
        .iter()
        .map(|&s| format!(r"\fB\-{}\fR", char::from(s)))
        .collect();
    names.extend(f.longs.iter().map(|l| format!(r"\fB\-\-{}\fR", roff(l))));
    let value = flag
        .value_name()
        .map_or_else(String::new, |v| format!(r" \fI{v}\fR"));
    _ = writeln!(out, ".TP\n{}{value}", names.join(", "));
    _ = writeln!(out, "{}", roff(flag.help().unwrap_or_default()));
    if let Some(env) = flag.env() {
        _ = writeln!(out, "[env: \\fB{}\\fR]", roff(env));
    }
}

pub fn man() -> String {
    let spec = Args::spec();
    let root = spec.root;
    let bin = spec.bin.unwrap_or(spec.name);
    let version = spec.version.unwrap_or_default();
    let mut out = format!(".TH {} 1 \"\" \"{bin} {version}\"\n", bin.to_uppercase());
    _ = writeln!(
        out,
        ".SH NAME\n{bin} \\- {}",
        roff(root.about().unwrap_or_default())
    );
    _ = writeln!(out, ".SH SYNOPSIS\n{}", synopsis(&[root]));
    _ = writeln!(out, ".SH OPTIONS");
    for f in root.flags().iter().filter(|f| !f.hide()) {
        flag(&mut out, f);
    }
    _ = writeln!(out, ".TP\n\\fB\\-h\\fR, \\fB\\-\\-help\\fR\nPrint help");
    _ = writeln!(
        out,
        ".TP\n\\fB\\-V\\fR, \\fB\\-\\-version\\fR\nPrint version"
    );
    _ = writeln!(out, ".SH COMMANDS");
    let global = |f: &FlagMeta| {
        root.flags()
            .iter()
            .any(|g| g.flag().longs == f.flag().longs)
    };
    for path in commands().iter().skip(1) {
        let cmd = path[path.len() - 1];
        if path.iter().any(|c| c.hide()) {
            continue;
        }
        let about = cmd.about().unwrap_or_default();
        _ = writeln!(out, ".TP\n{}\n{}", synopsis(path), roff(about));
        for f in cmd.flags().iter().filter(|f| !f.hide() && !global(f)) {
            out.push_str(".RS\n");
            flag(&mut out, f);
            out.push_str(".RE\n");
        }
        for arg in cmd.args() {
            if let Some(help) = arg.help() {
                let name = arg.arg().name;
                _ = writeln!(out, ".RS\n.TP\n\\fI{name}\\fR\n{}\n.RE", roff(help));
            }
        }
    }
    out
}

/// Writes the files to `A_GENERATE` when it is set; checks each script
/// with the shell that reads it, where this machine has that shell.
#[test]
fn completions_and_the_man_page_are_generated_from_the_spec() {
    let out = std::env::var_os("A_GENERATE");
    let scripts = SHELLS.map(|(shell, name, check)| (name, check, Args::completion_script(shell)));
    let files = scripts.into_iter().chain([("man/a.1", None, man())]);
    for (name, check, text) in files {
        assert!(
            !text.contains("__daemon"),
            "{name} shows the hidden command"
        );
        if let Some(dir) = &out {
            let path = Path::new(dir).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
            println!("{}", path.display());
        }
        let Some([program, flag]) = check else {
            continue;
        };
        let dir = std::env::temp_dir().join(format!("a-generate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name.rsplit('/').next().unwrap());
        std::fs::write(&file, &text).unwrap();
        if let Ok(status) = Command::new(program).arg(flag).arg(&file).status() {
            assert!(status.success(), "{name} does not parse:\n{text}");
        }
    }
    let man = man();
    for path in commands().iter().skip(1) {
        let cmd = path[path.len() - 1];
        if !cmd.hide() {
            assert!(man.contains(&roff(cmd.about().unwrap_or_default())));
        }
    }
}
