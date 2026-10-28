use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rt::{LoopbackListener, Runtime};

#[test]
fn loopback_listeners_are_ephemeral_and_release_their_port_with_their_owner() {
    let runtime = rt::Threads::new(1);
    let listener = LoopbackListener::bind(&runtime.reactor()).unwrap();
    let port = listener.port();
    let peer = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(&[42]).unwrap();
        let mut reply = [0];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [7]);
    });
    rt::block_on(async {
        let stream = listener.accept().await.unwrap();
        let mut byte = [0];
        assert_eq!(
            stream
                .read_with(|mut stream| stream.read(&mut byte))
                .await
                .unwrap(),
            1
        );
        assert_eq!(byte, [42]);
        stream.write_all(&[7]).await.unwrap();
    });
    peer.join().unwrap();
    drop(listener);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}
