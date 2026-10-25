use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
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

/// `group`'s leader and pipe as one image hands them to the next.
fn given(cmd: &mut Command, file: File) -> Given {
    let ContainedChild {
        child,
        group,
        capture,
    } = crate::spawn_group_captured(cmd, file).unwrap();
    let (pipe, unread) = park(capture.unwrap().reader).unwrap();
    assert_eq!(unread, Vec::<u8>::new());
    give(child, group, Some(pipe))
}

/// The process execs into a new image, and the group it started, with
/// what it wrote but nobody read yet, is the new image's: its exit status
/// is still there to wait for, and its pipe still holds its output.
// Miri runs no processes and validates no descriptors.
#[test]
#[cfg_attr(miri, ignore)]
fn an_exec_keeps_a_group_waitable_and_its_unread_output() {
    match std::env::var(STAGE).ok().as_deref() {
        Some("give") => {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let file = File::create(root.join("output")).unwrap();
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "echo before; sleep 1; echo after; exit 7"]);
            let given = given(&mut cmd, file);
            let owner = serde_json::json!({"call": 4});
            let path = root.join("record");
            let mut record = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            fs::remove_file(path).unwrap();
            record.write_all(&encode([(&given.group, &owner)])).unwrap();
            let mut next = rerun();
            next.env(STAGE, "adopt")
                .env(RECORD, record.as_raw_fd().to_string())
                .env(PID, std::process::id().to_string());
            let error = Successor::new(next)
                .handles(std::slice::from_ref(&record))
                .handles(&given.handles)
                .exec();
            panic!("exec failed: {error}");
        }
        Some("adopt") => {
            assert_eq!(std::env::var(PID).unwrap(), std::process::id().to_string());
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let number: i32 = std::env::var(RECORD).unwrap().parse().unwrap();
            let mut claimed = read_record(crate::InheritedNumber::from(number)).unwrap();
            let (claimed, owner) = claimed.pop().unwrap();
            assert_eq!(owner, serde_json::json!({"call": 4}));
            let file = File::options()
                .append(true)
                .open(root.join("output"))
                .unwrap();
            let ContainedChild {
                mut child,
                group: lease,
                capture,
            } = claimed.adopt(file);
            let OutputCapture {
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
#[cfg_attr(miri, ignore)]
fn a_handed_number_that_is_not_this_processs_child_is_refused() {
    let (reader, _writer) = io::pipe().unwrap();
    let group: Group = serde_json::from_value(serde_json::json!({
        "pid": 1,
        "output": std::os::fd::OwnedFd::from(reader).into_raw_fd(),
    }))
    .unwrap();
    let error = imp::claim(&group).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
}

/// A group the next image claims but no one adopts (its session is gone,
/// its tool is not there) does not outlive the claim: it is killed and
/// reaped as the claim drops.
#[test]
#[cfg_attr(miri, ignore)]
fn a_claimed_group_no_one_adopts_is_killed_and_reaped() {
    let path = std::env::temp_dir().join(format!("a-unadopted-{}", std::process::id()));
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "sleep 30"]);
    let given = given(&mut cmd, File::create(&path).unwrap());
    fs::remove_file(path).unwrap();
    let pid = given.group.pid;
    // As an exec passes them on: owned by nothing until claimed.
    for handle in given.handles {
        let _ = handle.into_raw_fd();
    }
    let claimed = imp::claim(&given.group).unwrap();
    assert!(crate::process_started(pid).is_some());
    drop(claimed);
    assert!(
        crate::process_started(pid).is_none(),
        "the group outlived its claim"
    );
}
