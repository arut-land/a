//! Shell completions and the man page, written from the command table.
//! Only the tests link them: `mise run generate:completions` and
//! `generate:man` run the test below with `A_GENERATE` set to the
//! directory they go to.

use std::fmt::Write;
use std::path::Path;
use std::process::Command;

use super::tests::commands;
use super::{BIN, Cmd, HELP, HELP_CMD, HOST, ROOT, VERSION, VERSION_FLAG};

/// The visible commands, each as its path from the root.
fn visible() -> Vec<Vec<&'static Cmd>> {
    let mut all = commands();
    all.retain(|path| path.iter().all(|cmd| !cmd.hidden));
    all
}

fn joined(path: &[&Cmd]) -> String {
    path.iter()
        .map(|cmd| cmd.name)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The flags that take a value, whose value a completer skips.
fn valued() -> String {
    let flags = commands()
        .into_iter()
        .flat_map(|path| path[path.len() - 1].flags);
    let mut longs: Vec<_> = flags
        .filter(|f| f.value.is_some())
        .map(|f| format!("--{}", f.long))
        .collect();
    longs.sort_unstable();
    longs.dedup();
    longs.join("|")
}

/// What may follow the command at `path`: subcommands, then flags, each
/// with its help.
fn candidates(path: &[&Cmd]) -> Vec<(String, &'static str)> {
    let cmd = path[path.len() - 1];
    let subs = cmd.subs.iter().filter(|sub| !sub.hidden);
    let mut out: Vec<_> = subs.map(|sub| (sub.name.to_owned(), sub.help)).collect();
    if !cmd.subs.is_empty() {
        out.push((HELP_CMD.0.to_owned(), HELP_CMD.1));
    }
    let flags = cmd
        .flags
        .iter()
        .chain(if path.len() > 1 { ROOT.flags } else { &[] });
    out.extend(flags.map(|f| (format!("--{}", f.long), f.help)));
    out.push(("--help".to_owned(), HELP.1));
    if path.len() == 1 {
        out.push(("--version".to_owned(), VERSION_FLAG.1));
    }
    out
}

/// `text` in single quotes, for any of the shells.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The loop both bash and zsh run over the words before the cursor: it
/// follows subcommand names down the table into `cmd`, skipping flag
/// values.
fn walk(out: &mut String, words: &str, from: usize, to: &str) {
    _ = writeln!(out, "  local cmd={BIN} i w");
    _ = writeln!(out, "  for ((i = {from}; i < {to}; i++)); do");
    _ = writeln!(out, "    w=${{{words}[i]}}");
    _ = writeln!(
        out,
        "    case $w in {}) ((i++)); continue ;; esac",
        valued()
    );
    _ = writeln!(out, "    case \"$cmd $w\" in");
    for path in visible().iter().skip(1) {
        let at = joined(path);
        _ = writeln!(out, "      {}) cmd={} ;;", quoted(&at), quoted(&at));
    }
    _ = writeln!(out, "    esac\n  done");
}

pub fn bash() -> String {
    let mut out = format!("# bash completion for {BIN}\n_{BIN}() {{\n");
    walk(&mut out, "COMP_WORDS", 1, "COMP_CWORD");
    _ = writeln!(out, "  local cur=${{COMP_WORDS[COMP_CWORD]}} words");
    _ = writeln!(out, "  case ${{COMP_WORDS[COMP_CWORD - 1]}} in");
    _ = writeln!(
        out,
        "    --{}) COMPREPLY=($(compgen -A hostname -- \"$cur\")); return ;;",
        HOST.long
    );
    _ = writeln!(out, "    {}) return ;;\n  esac\n  case $cmd in", valued());
    for path in visible() {
        let words: Vec<_> = candidates(&path)
            .into_iter()
            .map(|(word, _)| word)
            .collect();
        _ = writeln!(
            out,
            "    {}) words={} ;;",
            quoted(&joined(&path)),
            quoted(&words.join(" "))
        );
    }
    _ = writeln!(out, "  esac");
    _ = writeln!(
        out,
        "  COMPREPLY=($(compgen -W \"$words\" -- \"$cur\"))\n}}"
    );
    _ = writeln!(out, "complete -F _{BIN} {BIN}");
    out
}

pub fn zsh() -> String {
    let mut out = format!("#compdef {BIN}\n\n_{BIN}() {{\n");
    walk(&mut out, "words", 2, "CURRENT");
    _ = writeln!(out, "  case ${{words[CURRENT - 1]}} in");
    _ = writeln!(out, "    --{}) _hosts; return ;;", HOST.long);
    _ = writeln!(out, "    {}) return ;;\n  esac", valued());
    _ = writeln!(out, "  local -a subs flags\n  case $cmd in");
    for path in visible() {
        let (mut subs, mut flags) = (Vec::new(), Vec::new());
        for (word, help) in candidates(&path) {
            let row = quoted(&format!("{}:{help}", word.replace(':', r"\:")));
            if word.starts_with('-') {
                flags.push(row);
            } else {
                subs.push(row);
            }
        }
        _ = writeln!(out, "    {})", quoted(&joined(&path)));
        _ = writeln!(out, "      subs=({})", subs.join(" "));
        _ = writeln!(out, "      flags=({}) ;;", flags.join(" "));
    }
    _ = writeln!(out, "  esac");
    _ = writeln!(out, "  _describe -t commands command subs");
    _ = writeln!(out, "  _describe -t options flag flags\n}}\n");
    _ = writeln!(out, "if [[ $zsh_eval_context[-1] == loadautofunc ]]; then");
    _ = writeln!(out, "  _{BIN} \"$@\"\nelse\n  compdef _{BIN} {BIN}\nfi");
    out
}

pub fn fish() -> String {
    let mut out = format!("# fish completion for {BIN}\ncomplete -c {BIN} -f\n");
    for path in visible() {
        let cmd = path[path.len() - 1];
        // Where the command's own words go: right after its name, before
        // any of its subcommands.
        let after = if path.len() == 1 {
            "__fish_use_subcommand".to_owned()
        } else {
            let mut cond = format!("__fish_seen_subcommand_from {}", cmd.name);
            if !cmd.subs.is_empty() {
                let subs: Vec<_> = cmd.subs.iter().map(|sub| sub.name).collect();
                _ = write!(
                    cond,
                    "; and not __fish_seen_subcommand_from {}",
                    subs.join(" ")
                );
            }
            cond
        };
        for sub in cmd.subs.iter().filter(|sub| !sub.hidden) {
            _ = writeln!(
                out,
                "complete -c {BIN} -n {} -a {} -d {}",
                quoted(&after),
                sub.name,
                quoted(sub.help)
            );
        }
        if path.len() == 1 {
            for flag in ROOT.flags {
                let value = if flag.value.is_some() { " -r" } else { "" };
                _ = writeln!(
                    out,
                    "complete -c {BIN} -l {}{value} -d {}",
                    flag.long,
                    quoted(flag.help)
                );
            }
            _ = writeln!(out, "complete -c {BIN} -s h -l help -d {}", quoted(HELP.1));
            _ = writeln!(
                out,
                "complete -c {BIN} -n __fish_use_subcommand -s V -l version -d {}",
                quoted(VERSION_FLAG.1)
            );
            continue;
        }
        for flag in cmd.flags {
            let value = if flag.value.is_some() { " -r" } else { "" };
            _ = writeln!(
                out,
                "complete -c {BIN} -n {} -l {}{value} -d {}",
                quoted(&after),
                flag.long,
                quoted(flag.help)
            );
        }
    }
    out
}

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
fn synopsis(path: &[&Cmd]) -> String {
    let mut line = String::new();
    super::usage(&mut line, path);
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

pub fn man() -> String {
    let mut out = format!(".TH {} 1 \"\" \"{BIN} {VERSION}\"\n", BIN.to_uppercase());
    _ = writeln!(out, ".SH NAME\n{BIN} \\- {}", roff(ROOT.help));
    _ = writeln!(out, ".SH SYNOPSIS\n{}", synopsis(&[&ROOT]));
    _ = writeln!(out, ".SH OPTIONS");
    for flag in ROOT.flags {
        let value = flag
            .value
            .map_or_else(String::new, |v| format!(r" \fI{v}\fR"));
        _ = writeln!(
            out,
            ".TP\n\\fB\\-\\-{}\\fR{value}\n{}",
            roff(flag.long),
            roff(flag.help)
        );
        if let Some(env) = flag.env {
            _ = writeln!(out, "[env: \\fB{}\\fR]", roff(env));
        }
    }
    _ = writeln!(out, ".TP\n\\fB\\-h\\fR, \\fB\\-\\-help\\fR\n{}", HELP.1);
    _ = writeln!(
        out,
        ".TP\n\\fB\\-V\\fR, \\fB\\-\\-version\\fR\n{}",
        VERSION_FLAG.1
    );
    _ = writeln!(out, ".SH COMMANDS");
    for path in visible().iter().skip(1) {
        let cmd = path[path.len() - 1];
        _ = writeln!(out, ".TP\n{}\n{}", synopsis(path), roff(cmd.help));
        for flag in cmd.flags {
            _ = writeln!(
                out,
                ".RS\n.TP\n\\fB\\-\\-{}\\fR\n{}\n.RE",
                roff(flag.long),
                roff(flag.help)
            );
        }
        for arg in cmd.args.iter().filter(|arg| !arg.help.is_empty()) {
            _ = writeln!(
                out,
                ".RS\n.TP\n\\fI{}\\fR\n{}\n.RE",
                arg.name,
                roff(arg.help)
            );
        }
    }
    out
}

/// Every generated file, by its path under the output directory.
fn files() -> [(&'static str, String); 4] {
    [
        ("completions/a.bash", bash()),
        ("completions/a.zsh", zsh()),
        ("completions/a.fish", fish()),
        ("man/a.1", man()),
    ]
}

/// Writes the files to `A_GENERATE` when it is set; checks each with the
/// tool that reads it, where this machine has that tool.
#[test]
fn completions_and_the_man_page_are_generated_from_the_table() {
    let out = std::env::var_os("A_GENERATE");
    for (name, text) in files() {
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
        let check = match name.rsplit('.').next() {
            Some("bash") => ["bash", "-n"],
            Some("zsh") => ["zsh", "-n"],
            Some("fish") => ["fish", "--no-execute"],
            _ => continue,
        };
        let dir = std::env::temp_dir().join(format!("a-generate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name.rsplit('/').next().unwrap());
        std::fs::write(&file, &text).unwrap();
        if let Ok(status) = Command::new(check[0]).arg(check[1]).arg(&file).status() {
            assert!(status.success(), "{name} does not parse:\n{text}");
        }
    }
    let man = man();
    for path in visible().iter().skip(1) {
        assert!(man.contains(&roff(path[path.len() - 1].help)));
    }
}
