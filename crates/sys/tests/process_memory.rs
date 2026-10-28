//! Native process accounting must observe retained, touched allocations.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use sys::process_memory;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn retained_pages_increase_a_childs_charge() {
    let mut child = Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "charged_memory_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let ready = |output: &mut BufReader<_>| {
        let mut line = String::new();
        loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child exited: {line}"
            );
            if line.trim() == "ready" {
                break;
            }
            line.clear();
        }
    };
    ready(&mut output);
    let before = process_memory::charged_bytes(child.0.id()).unwrap();
    writeln!(child.0.stdin.as_mut().unwrap(), "allocate").unwrap();
    ready(&mut output);
    let after = process_memory::charged_bytes(child.0.id()).unwrap();
    assert!(
        after >= before + (3 << 20),
        "4 MiB retained allocation: {} {before} -> {after} bytes",
        process_memory::METRIC,
    );
}

#[test]
fn an_uninspectable_process_is_never_reported_as_zero() {
    assert!(process_memory::charged_bytes(0).is_err());
}

#[test]
#[ignore = "subprocess fixture"]
fn charged_memory_child() {
    println!("ready");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let retained = vec![0xA5u8; 4 << 20];
    std::hint::black_box(&retained);
    println!("ready");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_line(&mut line).unwrap();
    std::hint::black_box(retained);
}
