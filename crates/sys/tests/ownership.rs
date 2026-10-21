#![cfg(unix)]

use std::fs::File;
use std::os::fd::AsRawFd;
use sys::{InheritedNumber, Pass};

#[test]
fn invalid_inherited_numbers_never_create_owned_handles() {
    for number in [-1, i32::MIN, i32::MAX] {
        assert!(InheritedNumber::from(number).duplicate().is_err());
    }
    let (read, _write) = rustix::pipe::pipe().unwrap();
    let number = InheritedNumber::from(read.as_raw_fd());
    let home = sys::Home::from(std::env::temp_dir().join("a-c02-invalid"));
    assert!(sys::adopt_lock(number.duplicate().unwrap(), &home).is_err());
    assert!(rustix::io::fcntl_getfd(&read).is_ok());
}

#[test]
fn adoption_never_double_closes() {
    let dir = std::env::temp_dir().join(format!("a-c02-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("run")).unwrap();
    let home = sys::Home::from(dir.clone());
    let original = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(home.daemon_pid_file())
        .unwrap();
    sys::fs::try_lock(&original).unwrap();
    rustix::io::fcntl_setfd(&original, rustix::io::FdFlags::empty()).unwrap();
    let number = InheritedNumber::from(original.as_raw_fd());
    for _ in 0..2 {
        let owned = number.duplicate().unwrap();
        assert_ne!(owned.as_raw_fd(), original.as_raw_fd());
        for file in [&original, &owned] {
            assert!(
                rustix::io::fcntl_getfd(file)
                    .unwrap()
                    .contains(rustix::io::FdFlags::CLOEXEC)
            );
        }
        drop(sys::adopt_lock(owned, &home).unwrap());
        assert!(original.metadata().unwrap().is_file());
    }
    let listener = home.bind().unwrap();
    let number = InheritedNumber::from(listener.passed().to_string().parse::<i32>().unwrap());
    let adopted = sys::Listener::adopt(number.duplicate().unwrap(), &home.endpoint()).unwrap();
    drop(adopted);
    drop(home.endpoint().connect().unwrap());
    drop(listener.accept().unwrap());
    assert!(sys::Listener::adopt(original.try_clone().unwrap(), &home.endpoint()).is_err());
    assert!(original.metadata().is_ok());
    home.endpoint().remove();
    std::fs::remove_dir_all(dir).unwrap();
}
