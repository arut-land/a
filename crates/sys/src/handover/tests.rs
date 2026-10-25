use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;

use super::*;

const STAGE: &str = "A_TEST_HANDOVER_STAGE";
const ROOT: &str = "A_TEST_HANDOVER_ROOT";
const RECORD: &str = "A_TEST_HANDOVER_RECORD";
const PID: &str = "A_TEST_HANDOVER_PID";

fn rerun() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "handover::tests::an_exec_keeps_a_group_waitable_and_its_unread_output",
        "--nocapture",
    ]);
    command
}

/// The process execs into a new image, and the group it started, with
/// what it wrote but nobody read yet, is the new image's: its exit status
/// is still there to wait for, and its pipe still holds its output.
#[test]
fn an_exec_keeps_a_group_waitable_and_its_unread_output() {
    match std::env::var(STAGE).ok().as_deref() {
        Some("give") => {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let home = crate::Paths::from(root.clone());
            let file = File::create(root.join("output")).unwrap();
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "echo before; sleep 1; echo after; exit 7"]);
            let ContainedChild {
                child,
                group,
                capture,
            } = crate::spawn_group_captured(&mut cmd, file).unwrap();
            let capture = capture.unwrap();
            let given = give(&child, group, Some(capture.reader)).unwrap();
            let record = record(&home, &serde_json::to_vec(&given.group).unwrap()).unwrap();
            let mut next = rerun();
            next.env(STAGE, "adopt")
                .env(RECORD, record.as_raw_fd().to_string())
                .env(PID, std::process::id().to_string());
            let error = Successor::new(next)
                .handles(std::slice::from_ref(&record))
                .handles(&given.handles)
                .replace(&home);
            panic!("exec failed: {error}");
        }
        Some("adopt") => {
            assert_eq!(std::env::var(PID).unwrap(), std::process::id().to_string());
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let number: i32 = std::env::var(RECORD).unwrap().parse().unwrap();
            let bytes = read_record(crate::InheritedNumber::from(number)).unwrap();
            let group: Group = serde_json::from_slice(&bytes).unwrap();
            let file = File::options()
                .append(true)
                .open(root.join("output"))
                .unwrap();
            let ContainedChild {
                mut child,
                group: lease,
                capture,
            } = adopt(&group, file).unwrap();
            let crate::OutputCapture {
                mut reader,
                mut file,
            } = capture.unwrap();
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            file.write_all(&output).unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(7));
            drop(lease);
            assert_eq!(
                fs::read_to_string(root.join("output")).unwrap(),
                "before\nafter\n"
            );
            crate::child::mark("adopted");
        }
        _ => {
            let root = std::env::temp_dir().join(format!("a-handover-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let output = rerun()
                .env(STAGE, "give")
                .env(ROOT, &root)
                .output()
                .unwrap();
            fs::remove_dir_all(&root).unwrap();
            assert!(crate::child::ran(&output, "adopted"), "{output:?}");
        }
    }
}

#[test]
fn a_handed_number_that_is_not_this_processs_child_is_refused() {
    let (reader, _writer) = io::pipe().unwrap();
    let number = std::os::fd::OwnedFd::from(reader);
    let group: Group = serde_json::from_value(serde_json::json!({
        "pid": 1,
        "output": std::os::fd::IntoRawFd::into_raw_fd(number),
    }))
    .unwrap();
    let file = tempfile_in_temp();
    let error = adopt(&group, file).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
}

fn tempfile_in_temp() -> File {
    let path = std::env::temp_dir().join(format!("a-handover-refused-{}", std::process::id()));
    let file = File::create(&path).unwrap();
    fs::remove_file(path).unwrap();
    file
}
