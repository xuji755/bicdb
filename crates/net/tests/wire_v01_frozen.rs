//! **协议 v0.1 的字节级冻结**（与 `docs/客户端协议_v0.1.md` 的示例逐字节一致）。
//!
//! 为什么单写一个测试：驱动（Rust 的 `bicdb-client`、Python 的 `bicdb`）是
//! **各自实现的同一份规格**——规格示例一旦漂移，两边会"各自都能跑、互相说不通"。
//! 把示例钉成字面量，任何一个实现改动都会被这个测试拦住（Python 侧有同款断言，
//! 同一串字面量）。

use bicdb_net::message::{decode_statements, AuthOk, AuthRequest, Column, Statement};
use bicdb_net::{SqlRequest, Value};

/// 请求示例（`docs/客户端协议_v0.1.md` §7.1）。
fn example_request() -> SqlRequest {
    SqlRequest {
        sql: "SELECT id, name FROM t WHERE id = :id;".to_owned(),
        params: vec![("id".to_owned(), Value::Number("7".to_owned()))],
    }
}

/// **请求载荷**：60 字节，逐字节钉死。
#[test]
fn request_payload_is_byte_frozen() {
    let want = "P 1\nN 2\nid\nn1\n7\nQ 38\nSELECT id, name FROM t WHERE id = :id;\n";
    let got = example_request().encode();
    assert_eq!(got.len(), 60, "长度变了：{}", String::from_utf8_lossy(&got));
    assert_eq!(
        String::from_utf8_lossy(&got),
        want,
        "请求载荷与协议v0.1示例不符"
    );
    // 且**解得回去**。
    assert_eq!(SqlRequest::decode(&got).expect("解"), example_request());
}

/// **结果载荷**：85 字节，逐字节钉死。
#[test]
fn response_payload_is_byte_frozen() {
    let response = vec![Statement::Rows {
        columns: vec![
            Column {
                name: "ID".to_owned(),
                kind: 'n',
                nullable: true,
                type_code: 0,
                length: 0,
                type_name: String::new(),
            },
            Column {
                name: "NAME".to_owned(),
                kind: 'b',
                nullable: true,
                type_code: 0,
                length: 0,
                type_name: String::new(),
            },
        ],
        rows: vec![
            vec![
                Value::Number("7".to_owned()),
                Value::Bytes(b"alpha".to_vec()),
            ],
            vec![Value::Null, Value::Bytes("汉字".as_bytes().to_vec())],
        ],
    }];
    let want = "R 2 2\n\
                C n 1 0 0 2 0\nID\n\n\
                C b 1 0 0 4 0\nNAME\n\n\
                n1\n7\n\
                b10\n616c706861\n\
                -0\n\n\
                b12\ne6b189e5ad97\n";
    let got = bicdb_net::message::encode_statements(&response);
    assert_eq!(got.len(), 85, "长度变了：{}", String::from_utf8_lossy(&got));
    assert_eq!(
        String::from_utf8_lossy(&got),
        want,
        "结果载荷与协议v0.1示例不符"
    );
    assert_eq!(decode_statements(&got).expect("解"), response);
}

/// **帧**：`<首行>\n<载荷字节数>\n<载荷>`——载荷里可以有换行，靠长度切。
#[test]
fn frame_carries_a_payload_with_newlines() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let (mut a, mut b) = UnixStream::pair().expect("socketpair");
    let payload = "line1\nline2\n";
    bicdb_net::frame::write_frame(&mut a, "SQL", payload).expect("写帧");
    a.flush().expect("flush");
    let (head, body) = bicdb_net::frame::read_frame(&mut b).expect("读帧");
    assert_eq!(head, "SQL");
    assert_eq!(body, payload);
}

/// **半帧不算帧**：对端写一半就关，读侧要报错，不能"按行补出"半个结果。
#[test]
fn a_half_frame_is_an_error() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let (mut a, mut b) = UnixStream::pair().expect("socketpair");
    a.write_all(b"OK\n100\nhalf").expect("写");
    a.flush().expect("flush");
    drop(a); // 对端提前关：说 100 字节，只给了 4 字节
    let err = bicdb_net::frame::read_frame(&mut b).expect_err("应报错");
    assert!(format!("{err}").contains("连接"), "{err}");
}

#[test]
fn malformed_frame_lengths_are_rejected_by_both_drivers() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    for length in ["", "-1", "-0", "+1", "1_0", "67108865"] {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(format!("OK\n{length}\n").as_bytes()).unwrap();
        drop(a);
        assert!(
            matches!(
                bicdb_net::frame::read_frame_bytes(&mut b),
                Err(bicdb_net::frame::FrameError::Bad(_))
            ),
            "{length:?}"
        );
    }
}

#[test]
fn oversized_writes_do_not_send_a_partial_frame() {
    use std::os::unix::net::UnixStream;
    let (mut a, mut b) = UnixStream::pair().unwrap();
    a.set_write_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    let payload = vec![0; bicdb_net::frame::MAX_FRAME as usize + 1];
    assert!(matches!(
        bicdb_net::frame::write_frame_bytes(&mut a, "SQL", &payload),
        Err(bicdb_net::frame::FrameError::Bad(_))
    ));
    bicdb_net::frame::write_frame_bytes(&mut a, "SQL", b"ok").unwrap();
    assert_eq!(
        bicdb_net::frame::read_frame_bytes(&mut b).unwrap(),
        ("SQL".to_owned(), b"ok".to_vec())
    );
}

// ───────────────────── 协议 §7.3/§7.4：认证（D6） ─────────────────────

/// 认证请求示例（协议 §7.3）：主体 `alice`、口令 `s3cr3t`。
fn example_auth() -> AuthRequest {
    AuthRequest {
        user: "alice".to_owned(),
        password: "s3cr3t".to_owned(),
    }
}

/// **认证请求载荷**：21 字节，逐字节钉死（Python 侧同一串字面量）。
#[test]
fn auth_request_payload_is_byte_frozen() {
    let want = "U 5\nalice\nS 6\ns3cr3t\n";
    let got = example_auth().encode();
    assert_eq!(got.len(), 21, "长度变了：{}", String::from_utf8_lossy(&got));
    assert_eq!(
        String::from_utf8_lossy(&got),
        want,
        "与协议v0.1 §7.3 示例不符"
    );
    assert_eq!(AuthRequest::decode(&got).expect("解"), example_auth());
}

/// **认证应答载荷**：35 字节，逐字节钉死。
#[test]
fn auth_reply_payload_is_byte_frozen() {
    let ok = AuthOk {
        user: "alice".to_owned(),
        user_id: 7,
        expired: false,
    };
    let want = "user=alice\nuser_id=7\nstatus=active\n";
    assert_eq!(ok.encode().len(), 35);
    assert_eq!(ok.encode(), want);
    assert_eq!(AuthOk::decode(&ok.encode()), ok);
    // 过期形态只差状态词（受限会话；协议 §4.3）。
    let expired = AuthOk {
        expired: true,
        ..ok.clone()
    };
    assert_eq!(expired.encode(), want.replace("active", "expired"));
    assert!(AuthOk::decode(&expired.encode()).expired);
}
