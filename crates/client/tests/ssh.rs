//! An ssh link reports why it could not open as a typed error, through a
//! fake `ssh` (`A_SSH`) that fails as the real one does. Its own test
//! binary, with one test, so setting `A_SSH` races no other thread.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::{fs, process};

use client::{Daemon, Error, LinkError, SshLink};
use sys::{Home, Prompt};

#[test]
fn a_link_that_may_not_prompt_says_a_person_must_authenticate_instead_of_asking() {
    let root = std::env::temp_dir().join(format!("a-client-ssh-{}", process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let home = Home::from(root.join("home"));
    let exe = root.join("a");
    fs::write(&exe, "a build that is never run").unwrap();
    let staged = home.runtime().stage(&exe).unwrap();
    // As OpenSSH fails: in batch mode it may not ask for the password;
    // otherwise the host does not answer.
    let ssh = root.join("ssh");
    fs::write(
        &ssh,
        r#"#!/bin/sh
case " $* " in
*" BatchMode=yes "*) echo "box: Permission denied (publickey,password)." >&2 ;;
*) echo "ssh: connect to host box port 22: Connection refused" >&2 ;;
esac
exit 255
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    #[allow(unsafe_code)]
    // SAFETY: this binary's one test sets it before any thread reads it.
    unsafe {
        std::env::set_var("A_SSH", &ssh);
    }

    let batch = Daemon::new(
        SshLink::new("box".parse().unwrap(), Prompt::Never, home.clone()),
        staged.clone(),
    );
    assert!(!batch.link.needs_terminal());
    let err = batch.connect(&|| {}).err().expect("ssh failed");
    assert!(
        matches!(&err, Error::Link(LinkError::NeedsInteractiveAuth(why)) if why.contains("ssh box")),
        "{err:?}"
    );

    let terminal = Daemon::new(
        SshLink::new("box".parse().unwrap(), Prompt::Terminal, home),
        staged,
    );
    assert!(terminal.link.needs_terminal());
    let err = terminal.connect(&|| {}).err().expect("ssh failed");
    assert!(
        matches!(err, Error::Link(LinkError::Unreachable(_))),
        "{err:?}"
    );
    fs::remove_dir_all(&root).unwrap();
}
