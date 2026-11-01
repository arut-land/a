use std::io::Write;

#[test]
fn an_ordered_link_keeps_identity_and_later_appends_after_the_source_name_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let retained = dir.path().join("retained");
    let mut file = std::fs::File::create(&source).unwrap();
    file.write_all(b"first").unwrap();
    file.sync_all().unwrap();
    sys::fs::link_ordered(&source, &retained).unwrap();
    assert_eq!(
        sys::fs::identity(&source).unwrap(),
        sys::fs::identity(&retained).unwrap()
    );
    file.write_all(b" second").unwrap();
    file.sync_all().unwrap();
    drop(file);
    std::fs::remove_file(source).unwrap();
    assert_eq!(std::fs::read(retained).unwrap(), b"first second");
}

#[test]
fn an_ordered_link_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::write(&source, b"source").unwrap();
    std::fs::write(&target, b"target").unwrap();
    assert_eq!(
        sys::fs::link_ordered(&source, &target).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(source).unwrap(), b"source");
    assert_eq!(std::fs::read(target).unwrap(), b"target");
}
