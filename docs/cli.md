# `a` in a pipeline (`apps/cli`)

Which client `a` (and `a resume`, `a attach`) runs depends on its stdio:

| stdin | stdout | runs |
| --- | --- | --- |
| terminal | terminal | the full-screen client ([tui.md](tui.md)); `A_TUI=inline` the inline one |
| terminal | pipe or file, or `TERM=dumb` | the line REPL: plain lines, no escapes |
| pipe or file | any | one turn, then exit (below) |
| any, with `-p` | any | one turn, then exit |

`A_TUI=plain` asks for the line REPL whatever stdio is, so a program can
drive it over pipes (the black-box tests do).

## One turn

```sh
echo "what does this repo do?" | a
a < question.md
a -p "summarise" < build.log
git diff | a -p "review this"
a -p "and the tests?" resume        # one more turn in the latest session
```

- The prompt is `-p`'s text, then all of stdin after a blank line, either
  alone when the other is missing or blank. All of stdin is one prompt, as
  with `claude -p`: a file is read whole, not a line at a time (the line
  REPL is the way to send one message per line). Text starting `/` is a
  slash command, as typed.
- It runs in a new session, or the one `resume`/`attach` names, which stays
  in the daemon like any other: `a ls` lists it, `a attach` opens it.
- stdout gets what the agent says, streamed as it is written: every reply's
  text, each ending in a newline. Nothing else goes there: no session line,
  tool calls or their output, escapes, spinners or status. Notes (an
  upgrade reconnecting, why it failed) go to stderr.
- A run with no terminal never waits for another client. A call that needs
  approval ends it (status 3) with a message on stderr naming the tool and
  the session, and the ways on: answer it with `a attach <id>` or in
  `a web` (the question stays open in the session), or run again with
  `--mode auto`. `--mode` (or `A_MODE`) sets the session's approval mode
  before the prompt goes, as the policy's `/mode` would: `ask`,
  `accept-edits`, `auto` or `judge`. What the operator's policy floor asks
  about or denies still is.
- Ctrl-C (SIGINT) interrupts the turn and exits 130.
- A reader that goes (`a … | head -1`) ends `a` quietly with 141, the
  status SIGPIPE gives a program; the turn goes on in the daemon. `a ls`,
  the line REPL and the rest end the same way.

## Exit status

| status | the turn |
| --- | --- |
| 0 | ran to its end |
| 1 | failed: a model call failed, the daemon refused the input or went away, no such session, or `--mode` named no mode |
| 2 | was never sent: a usage error, or nothing to send |
| 3 | waits on someone (an approval, a question); the session keeps the question |
| 130 | was interrupted: Ctrl-C here, or by another client |
| 141 | stdout's reader went away |

No output format flag: stdout is the reply, and scripts that want the
structure read the session's log (`$A_HOME/sessions/<id>.jsonl`) or speak
the protocol (`a bridge`).
