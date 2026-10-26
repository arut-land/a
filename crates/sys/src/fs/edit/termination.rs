//! Process termination checks the externally visible namespace at publication boundaries.
use super::*;
use std::io::BufRead;
use std::process::{Command, Stdio};

#[test]
fn publication_child() {
    let Some(root) = std::env::var_os("A_EDIT_PUBLICATION_ROOT") else {
        return;
    };
    let target = PathBuf::from(root).join("target");
    let exchange = std::env::var_os("A_EDIT_PUBLICATION_EXCHANGE").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let conflict = std::env::var_os("A_EDIT_PUBLICATION_CONFLICT").is_some();
    let transaction = EditTransaction::capture(&target, Some(1024)).unwrap();
    publication_step("captured");
    let result = transaction.publish_with_sync(
        b"new",
        if cfg!(unix) {
            Persistence::Durable
        } else {
            Persistence::Visible
        },
        File::sync_all,
        &|_| false,
        exchange,
        || {
            if conflict {
                let incoming = target.with_file_name("incoming");
                fs::write(&incoming, b"theirs").unwrap();
                super::super::replace(&incoming, &target).unwrap();
            }
        },
    );
    assert!(result.is_ok() || conflict, "{result:?}");
    panic!("publication did not reach its requested checkpoint");
}

struct Workspace(PathBuf);
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn terminate_at(step: &str, exchange: bool, conflict: bool) {
    let work = Workspace(std::env::temp_dir().join(temporary_name()));
    fs::create_dir(&work.0).unwrap();
    let target = work.0.join("target");
    fs::write(&target, b"old").unwrap();
    let test = format!(
        "{}::publication_child",
        module_path!().split_once("::").unwrap().1
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env("A_EDIT_PUBLICATION_ROOT", &work.0)
        .env("A_EDIT_PUBLICATION_STEP", step)
        .env(
            "A_EDIT_PUBLICATION_EXCHANGE",
            if exchange { "1" } else { "0" },
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if conflict {
        command.env("A_EDIT_PUBLICATION_CONFLICT", "1");
    }
    let mut child = command.spawn().unwrap();
    let output = child.stdout.take().unwrap();
    let (send, received) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let ready = io::BufReader::new(output)
            .lines()
            .any(|line| line.is_ok_and(|line| line.contains("EDIT_AT:")));
        let _ = send.send(ready);
    });
    let ready = received.recv_timeout(std::time::Duration::from_secs(60));
    let _ = child.kill();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert!(
        matches!(ready, Ok(true)),
        "{step}: checkpoint not reached ({ready:?}, {status})"
    );
    let content = fs::read(&target).unwrap_or_else(|error| panic!("{step}, exchange={exchange}, conflict={conflict}: target absent after termination: {error}"));
    assert!(
        content == b"old" || content == b"new" || (conflict && content == b"theirs"),
        "{step}: torn content {content:?}"
    );
    if conflict {
        // The competing replacement is never lost: it is published or staged.
        let staged = fs::read_dir(&work.0)
            .unwrap()
            .filter_map(|entry| fs::read(entry.ok()?.path().join("content")).ok());
        assert!(
            content == b"theirs" || staged.into_iter().any(|bytes| bytes == b"theirs"),
            "{step}: the competing replacement was lost"
        );
    }
}

#[test]
fn termination_at_every_publication_step_keeps_the_target_present_and_whole() {
    for step in [
        "captured",
        "staged",
        "written",
        "metadata",
        "file_sync",
        "final_check",
        "before_replace",
        "after_replace",
        "parent_sync",
        "cleanup",
    ] {
        terminate_at(step, false, false);
        #[cfg(unix)]
        terminate_at(step, true, false);
    }
    #[cfg(unix)]
    for conflict in [false, true] {
        terminate_at("displaced_checked", true, conflict);
        if conflict {
            terminate_at("before_rollback", true, true);
            terminate_at("after_rollback", true, true);
        }
    }
}
