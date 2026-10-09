//! The native daemon rejects queued connections with an explicit busy frame.
use bicdb_net::{frame, Client, ClientError};
use std::os::unix::net::UnixListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn handshake_busy_frame_is_retryable_but_other_server_errors_are_not() {
    for (message, busy) in [("实例正忙：稍后重试", true), ("版本不兼容", false)] {
        let path = std::env::temp_dir().join(format!(
            "bicdb-busy-{}-{}.sock",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            frame::read_frame_bytes(&mut stream).unwrap();
            frame::write_frame_bytes(&mut stream, "ERR", message.as_bytes()).unwrap();
        });
        let result = Client::connect_with_timeout(&path, Duration::from_secs(2));
        assert!(matches!(result, Err(ClientError::Busy)) == busy);
        if !busy {
            assert!(matches!(result, Err(ClientError::Server(_))));
        }
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
