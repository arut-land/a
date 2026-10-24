//! Blob references as the wire and the log carry them.

use protocol::BlobId;

#[test]
fn a_blob_id_is_its_hex_both_ways() {
    let id = BlobId(std::array::from_fn(|i| u8::try_from(i * 7).unwrap()));
    let text = id.to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text.parse(), Ok(id));
    assert_eq!(id.fan(), &text[..2]);
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(serde_json::from_str::<BlobId>(&json).unwrap(), id);
    for bad in [
        "",
        "AB",
        &text.to_uppercase(),
        &text[1..],
        &format!("{text}0"),
    ] {
        assert!(bad.parse::<BlobId>().is_err(), "{bad}");
    }
}
