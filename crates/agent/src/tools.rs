use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Cx, Replay, Tool};

pub struct RunCommand;

impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn call(&self, input: &Value, cx: &Cx) -> Result<Value, String> {
        let command = input["command"].as_str().ok_or("missing `command`")?;
        let shell = if cfg!(windows) {
            ["cmd", "/c"]
        } else {
            ["sh", "-c"]
        };
        let out = Command::new(shell[0])
            .args([shell[1], command])
            .current_dir(&cx.cwd)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "exit": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }))
    }
}

pub struct Sleep;

impl Tool for Sleep {
    fn name(&self) -> &'static str {
        "sleep"
    }

    fn replay(&self) -> Replay {
        Replay::Safe
    }

    fn call(&self, input: &Value, _cx: &Cx) -> Result<Value, String> {
        let seconds = input["seconds"].as_u64().ok_or("missing `seconds`")?;
        thread::sleep(Duration::from_secs(seconds));
        Ok(json!({ "slept": seconds }))
    }
}
