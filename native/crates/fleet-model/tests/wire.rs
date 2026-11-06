//! The terminal frames through `write_frame` and `read_frame`, and the reader's limit on a `TermIn`.

use fleet_model::types::AttachTarget;
use fleet_model::wire::{
    read_frame, write_frame, Frame, Role, PROTO, TERM_CLIENT_MAX, TERM_IN_MAX,
};

fn terminal_frames() -> Vec<Frame> {
    vec![
        Frame::Hello {
            proto: PROTO,
            since: None,
            run: None,
            role: Role::Terminals,
        },
        Frame::TermOpen {
            r#ref: 1,
            target: AttachTarget::Claude {
                profile: "default".into(),
                job: "a1b2c3d4".into(),
            },
            cols: 100,
            rows: 30,
        },
        Frame::TermOpen {
            r#ref: 2,
            target: AttachTarget::Codex {
                profile: "codex".into(),
                session_id: "0192f3a4-5b6c-7d8e-9f01-23456789abcd".into(),
                cwd: "/tmp/fleetd-fixture".into(),
            },
            cols: 80,
            rows: 24,
        },
        Frame::TermOpen {
            r#ref: 3,
            target: AttachTarget::Trust {
                profile: "default".into(),
                dir: "/tmp/fleetd-fixture".into(),
            },
            cols: 400,
            rows: 200,
        },
        Frame::TermIn {
            r#ref: 1,
            data: vec![0, 1, 2, 255],
        },
        Frame::TermSize {
            r#ref: 1,
            cols: 132,
            rows: 43,
        },
        Frame::TermClose { r#ref: 1 },
        Frame::TermOut {
            r#ref: 1,
            data: b"hello\r\n".to_vec(),
        },
        Frame::TermEnd {
            r#ref: 1,
            error: None,
        },
        Frame::TermEnd {
            r#ref: 2,
            error: Some("gone".into()),
        },
    ]
}

#[tokio::test]
async fn every_terminal_frame_round_trips() {
    for f in terminal_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).await.unwrap();
        let got = read_frame(&mut buf.as_slice(), TERM_CLIENT_MAX)
            .await
            .unwrap();
        assert_eq!(got, f);
    }
}

#[tokio::test]
async fn a_term_in_over_64_kib_is_refused_by_the_reader_limit() {
    let at_limit = Frame::TermIn {
        r#ref: 1,
        data: vec![b'x'; TERM_IN_MAX],
    };
    let mut buf = Vec::new();
    write_frame(&mut buf, &at_limit).await.unwrap();
    assert_eq!(
        read_frame(&mut buf.as_slice(), TERM_CLIENT_MAX)
            .await
            .unwrap(),
        at_limit
    );

    let over = Frame::TermIn {
        r#ref: 1,
        data: vec![b'x'; TERM_IN_MAX + 1024],
    };
    let mut buf = Vec::new();
    write_frame(&mut buf, &over).await.unwrap();
    let e = read_frame(&mut buf.as_slice(), TERM_CLIENT_MAX)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}
