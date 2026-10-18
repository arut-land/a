//! The built-in tools.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};
use std::process::{Command as Process, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Cx, Replay, Tool};

/// A JSON Schema object taking exactly `properties`, all required.
fn object(properties: &Value) -> Value {
    let required: Vec<&String> = properties
        .as_object()
        .into_iter()
        .flat_map(|p| p.keys())
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub struct RunCommand;

#[derive(Deserialize)]
pub struct Command {
    pub command: String,
}

#[derive(Serialize, Deserialize)]
pub struct Ran {
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Tool for RunCommand {
    type In = Command;
    type Out = Ran;

    const NAME: &'static str = "run_command";
    const DESCRIPTION: &'static str = "Runs a shell command in the user's working directory and environment and returns its exit code, stdout and stderr.";

    fn schema() -> Value {
        object(&json!({ "command": { "type": "string", "description": "The command line." } }))
    }

    async fn call(&self, input: Command, cx: &Cx<'_>) -> Result<Ran, String> {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/c")
        } else {
            ("sh", "-c")
        };
        let mut cmd = Process::new(shell);
        let mut cwd = cx.cwd;
        if let Some(env) = cx.env {
            cmd.env_clear().envs(&env.vars);
            // A directory removed since the capture falls back to the
            // workspace rather than failing every command.
            if env.cwd.is_dir() {
                cwd = &env.cwd;
            }
        }
        cmd.args([flag, &input.command])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        (cx.spawned)(child.id());
        let out = rt::unblock(cx.rt, move || child.wait_with_output())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Ran {
            exit: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

pub struct Sleep;

#[derive(Deserialize)]
pub struct Seconds {
    pub seconds: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Slept {
    pub slept: u64,
}

impl Tool for Sleep {
    type In = Seconds;
    type Out = Slept;

    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "Waits for a number of seconds.";
    const REPLAY: Replay = Replay::Safe;

    fn schema() -> Value {
        object(&json!({ "seconds": { "type": "integer", "minimum": 0 } }))
    }

    async fn call(&self, input: Seconds, cx: &Cx<'_>) -> Result<Slept, String> {
        cx.rt.sleep(Duration::from_secs(input.seconds)).await;
        Ok(Slept {
            slept: input.seconds,
        })
    }
}

pub struct ReadFile;

/// The most of a file one read returns, so a large file cannot flood the
/// log or the model's context.
pub const READ_LIMIT: usize = 128 * 1024;

#[derive(Deserialize)]
pub struct FilePath {
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct FileText {
    pub content: String,
    /// The file is longer than [`READ_LIMIT`] bytes; `content` is its start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl Tool for ReadFile {
    type In = FilePath;
    type Out = FileText;

    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str =
        "Reads a text file, by its path relative to the workspace. Returns at most 128 KiB.";
    const REPLAY: Replay = Replay::Safe;

    fn schema() -> Value {
        object(
            &json!({ "path": { "type": "string", "description": "Relative to the workspace." } }),
        )
    }

    async fn call(&self, input: FilePath, cx: &Cx<'_>) -> Result<FileText, String> {
        let path = Path::new(&input.path);
        if !path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(format!("{} is not a path inside the workspace", input.path));
        }
        let path = cx.cwd.join(path);
        let mut bytes = rt::unblock(cx.rt, move || {
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| file.take(READ_LIMIT as u64 + 1).read_to_end(&mut bytes))
                .map(|_| bytes)
        })
        .await
        .map_err(|e| format!("{}: {e}", input.path))?;
        let truncated = bytes.len() > READ_LIMIT;
        bytes.truncate(READ_LIMIT);
        Ok(FileText {
            content: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        })
    }

    fn to_model(output: &FileText) -> llm::Output {
        let mut text = output.content.clone();
        if output.truncated {
            text.push_str("\n[truncated at 128 KiB]");
        }
        llm::Output::Text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DynTool, Toolset};

    static RT: std::sync::LazyLock<rt::Threads> = std::sync::LazyLock::new(rt::Threads::default);

    fn cx(cwd: &Path) -> Cx<'_> {
        Cx {
            cwd,
            env: None,
            spawned: &|_| {},
            rt: &*RT,
        }
    }

    fn call(tool: &dyn DynTool, input: &Value, cx: &Cx<'_>) -> protocol::Outcome {
        rt::block_on(tool.call(input, cx))
    }

    #[test]
    fn commands_run_in_the_clients_environment() {
        let dir = std::env::temp_dir();
        let sub = dir.join(format!("a-run-env-{}", std::process::id()));
        std::fs::create_dir_all(&sub).unwrap();
        let env = protocol::Environment {
            cwd: std::fs::canonicalize(&sub).unwrap(),
            vars: [("PATH", "/usr/bin:/bin"), ("ONLY_HERE", "1")]
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .into(),
            ..Default::default()
        };
        let cx = Cx {
            env: Some(&env),
            ..cx(&dir)
        };
        let script = json!({ "command": "echo \"$ONLY_HERE $HOME\"; pwd -P" });
        let ran = call(&RunCommand, &script, &cx);
        std::fs::remove_dir_all(&sub).unwrap();
        let protocol::Outcome::Done { output } = ran else {
            panic!("{ran:?}")
        };
        assert_eq!(
            output["stdout"],
            format!("1 \n{}\n", env.cwd.display()),
            "only the captured variables, in the captured directory"
        );
    }

    #[test]
    fn schemas_name_every_input_field() {
        for tool in Toolset::default().0 {
            let spec = tool.spec();
            assert_eq!(spec.schema["type"], "object", "{}", spec.name);
            assert_ne!(spec.description, "");
        }
        assert_eq!(RunCommand::schema()["required"], json!(["command"]));
    }

    #[test]
    fn inputs_are_typed() {
        let dir = std::env::temp_dir();
        let outcome = call(&RunCommand, &json!({ "cmd": "true" }), &cx(&dir));
        assert!(
            matches!(outcome, protocol::Outcome::Failed { error } if error.starts_with("invalid input"))
        );
        let ran = call(&RunCommand, &json!({ "command": "echo hi" }), &cx(&dir));
        assert!(
            matches!(ran, protocol::Outcome::Done { output } if output["stdout"] == "hi\n" && output["exit"] == 0)
        );
    }

    #[test]
    fn read_file_stays_in_the_workspace_and_is_bounded() {
        let dir = std::env::temp_dir().join(format!("a-read-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.txt"), "hello").unwrap();
        std::fs::write(dir.join("big.txt"), vec![b'x'; READ_LIMIT + 10]).unwrap();
        let read = |path: &str| call(&ReadFile, &json!({ "path": path }), &cx(&dir));
        let small = read("small.txt");
        let big = read("./big.txt");
        let outside = [read("../small.txt"), read("/etc/hosts")];
        let missing = read("missing.txt");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            small,
            protocol::Outcome::Done {
                output: json!({ "content": "hello" })
            }
        );
        let protocol::Outcome::Done { output } = big else {
            panic!("{big:?}")
        };
        assert_eq!(output["truncated"], true);
        assert_eq!(output["content"].as_str().unwrap().len(), READ_LIMIT);
        assert!(matches!(
            DynTool::to_model(&ReadFile, &output),
            llm::Output::Text(text) if text.ends_with("[truncated at 128 KiB]")
        ));
        for outcome in outside.into_iter().chain([missing]) {
            assert!(
                matches!(outcome, protocol::Outcome::Failed { .. }),
                "{outcome:?}"
            );
        }
    }
}
