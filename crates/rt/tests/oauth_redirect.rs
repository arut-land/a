use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rt::{OAuthRedirect, Runtime};

#[test]
fn a_loopback_callback_uses_an_ephemeral_port_and_releases_it_with_its_owner() {
    let runtime = rt::Threads::new(1);
    let listener = OAuthRedirect::bind(&runtime.reactor()).unwrap();
    let uri = listener.uri();
    let address = uri
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let browser = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(
                b"GET /oauth/callback?code=secret&state=nonce HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"));
        assert!(!reply.contains("secret"));
    });
    assert_eq!(
        rt::block_on(listener.receive()).unwrap(),
        "code=secret&state=nonce"
    );
    browser.join().unwrap();
    drop(listener);
    assert!(
        TcpStream::connect(
            uri.strip_prefix("http://")
                .unwrap()
                .split('/')
                .next()
                .unwrap()
        )
        .is_err()
    );
}
