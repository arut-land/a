//! How blobs are named: their places in the tree, and as the model reads them.

use std::path::{Path, PathBuf};

use ext::blob::{BlobId, BlobOrigin, BlobRef, Places, TreePath};
use protocol::{CallId, OutputStream, SessionId};

#[test]
fn a_reference_is_named_by_its_place_in_the_tree() {
    let blob = BlobRef {
        id: BlobId([1; 32]),
        size: 1,
        media_type: "text/plain".into(),
        origin: BlobOrigin::Paste,
    };
    let stream = OutputStream {
        call: CallId::new(4).unwrap(),
    };
    let bare = Places::default();
    assert_eq!(bare.blob(&blob), format!("by-id/{}", blob.id));
    assert_eq!(bare.stream(stream), "stream:4");
    let session = SessionId::new();
    let placed = Places::new(PathBuf::from("root"), session);
    assert_eq!(
        placed.stream(stream),
        Path::new("root")
            .join("sessions")
            .join(session.to_string())
            .join("streams")
            .join("4.log")
            .display()
            .to_string()
    );
}

#[test]
fn a_tree_path_never_leaves_its_root() {
    for bad in [
        "..", "a/../b", "/a", "a//b", "a/", "./a", "a\\b", "c:x", "a\0",
    ] {
        assert_eq!(TreePath::new(bad), None, "{bad:?}");
    }
    let at = TreePath::new("sessions/x/tool-output/7.log").unwrap();
    let root = Path::new("root");
    assert_eq!(
        at.under(root),
        root.join("sessions")
            .join("x")
            .join("tool-output")
            .join("7.log")
    );
    assert_eq!(TreePath::ROOT.under(root), root);
    let id = BlobId([0xab; 32]);
    assert_eq!(
        TreePath::by_id(&id).parts().collect::<Vec<_>>(),
        ["by-id", &id.to_string()]
    );
}
