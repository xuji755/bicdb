//! Idle callbacks must preserve fragmented frames without changing wire bytes.
use bicdb_net::frame::{self, FrameError};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn timeouts_preserve_partial_headers_and_binary_payload() {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let writer = std::thread::spawn(move || {
        for fragment in [&b"SQ"[..], b"L\n", b"5", b"\n\0", b"\xff\n", b"ab"] {
            client.write_all(fragment).unwrap();
            std::thread::sleep(Duration::from_millis(35));
        }
        frame::write_frame_bytes(&mut client, "HELLO", b"next").unwrap();
    });
    let mut callbacks = 0;
    let got = frame::read_frame_bytes_with_idle(&mut server, || callbacks += 1).unwrap();
    assert_eq!(got, ("SQL".into(), vec![0, 255, b'\n', b'a', b'b']));
    assert!(callbacks >= 5);
    assert_eq!(
        frame::read_frame_bytes_with_idle(&mut server, || {}).unwrap(),
        ("HELLO".into(), b"next".to_vec())
    );
    writer.join().unwrap();
}

#[test]
fn eof_and_oversized_frames_remain_errors_after_timeouts() {
    for partial in [&b"SQL\n5\nab"[..], b"SQL\n67108865\n", b"SQL\nx\n"] {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let payload = partial.to_vec();
        let writer = std::thread::spawn(move || {
            client.write_all(b"S").unwrap();
            std::thread::sleep(Duration::from_millis(40));
            client.write_all(&payload[1..]).unwrap();
        });
        let mut callbacks = 0;
        let got = frame::read_frame_bytes_with_idle(&mut server, || callbacks += 1);
        assert!(callbacks > 0);
        assert!(matches!(
            got,
            Err(FrameError::Bad(_)) | Err(FrameError::Io(_))
        ));
        writer.join().unwrap();
    }
}
