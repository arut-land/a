//! Process-entry ownership of resources inherited without Rust owners.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Claims, claim_record, decode, imp, validate};
use crate::InheritedNumber;

/// The handover claimed at process entry, consumed once by daemon startup.
pub struct Startup(Option<(InheritedNumber, io::Result<Claims>)>);

impl Startup {
    /// Returns the handover captured at entry for this command-line number.
    pub fn read_record(self, number: InheritedNumber) -> io::Result<Claims> {
        match self.0 {
            Some((expected, record)) if expected == number => record,
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no inherited handover under that number at process entry",
            )),
        }
    }
}

/// Defines the native entry point before application code can own resources.
#[macro_export]
macro_rules! main {
    ($run:path) => {
        fn main() -> ::std::process::ExitCode {
            let run: fn($crate::handover::Startup) -> ::std::process::ExitCode = $run;
            // SAFETY: this is native process entry, before application resource
            // owners exist. enter rejects recursive calls to this entry point.
            unsafe { $crate::handover::enter(run) }
        }
    };
}

/// # Safety
/// Call only at native process entry, before application resource owners exist.
/// The generated [`crate::main!`] entry point enforces this ordering.
#[doc(hidden)]
pub unsafe fn enter(run: fn(Startup) -> ExitCode) -> ExitCode {
    static ENTERED: AtomicBool = AtomicBool::new(false);
    assert!(
        !ENTERED.swap(true, Ordering::AcqRel),
        "process entry runs once"
    );
    let mut args = std::env::args_os().skip(1);
    let number = if args.next().is_some_and(|arg| arg == "__daemon") {
        args.find_map(|arg| {
            if arg == "--handover-fd" {
                Some(None)
            } else {
                arg.to_str()?
                    .strip_prefix("--handover-fd=")
                    .map(|n| Some(n.to_owned()))
            }
        })
        .and_then(|number| number.or_else(|| args.next()?.into_string().ok()))
        .and_then(|number| number.parse::<i32>().ok())
        .map(InheritedNumber::from)
    } else {
        None
    };
    let record = number.map(|number| {
        // SAFETY: application code has not run, so inherited resources have
        // no Rust owners. The record's distinct originals are taken only once.
        (number, unsafe { read_inherited_record(number) })
    });
    run(Startup(record))
}

/// # Safety
/// The record and the inherited resources it names must have no Rust owners.
/// Call once, before application code can adopt or close these resources.
pub(crate) unsafe fn read_inherited_record(number: InheritedNumber) -> io::Result<Claims> {
    // SAFETY: the caller establishes exclusive ownership of inherited originals.
    let original = unsafe { imp::take_inherited(number.raw()) }?;
    let file = original.try_clone();
    drop(original);
    let record = decode(file?)?;
    let mut originals = HashMap::<i32, File>::new();
    for raw in record.groups.iter().flat_map(|entry| entry.group.numbers()) {
        if raw == number.raw() || originals.contains_key(&raw) {
            continue;
        }
        // SAFETY: the caller guarantees these inherited originals have no
        // Rust owners; the map gives each distinct number exactly one owner.
        if let Ok(file) = unsafe { imp::take_inherited(raw) } {
            originals.insert(raw, file);
        }
    }
    validate(&record, number)?;
    Ok(claim_record(record, |number| {
        let original = originals.remove(&number).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "no inherited resource under that number",
            )
        })?;
        let duplicate = original.try_clone();
        drop(original);
        duplicate
    }))
}
