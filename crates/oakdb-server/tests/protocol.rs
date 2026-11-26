//! The frames of the wire protocol (design 0020, sections 2 to 4;
//! docs/protocol.md): what each one looks like in bytes, that every frame
//! decodes back to what was encoded, and that no stream of bytes makes the
//! decoder panic.

use oakdb_server::protocol::{
    closes, code, command, error_code, read_frame, write_frame, DecodeError, Frame, FrameError,
    Request, Response, MAX_FRAME_LEN, VERSION,
};
use oakdb_sql::{Output, SqlError, Value};
use proptest::prelude::*;

fn bytes_of(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    write_frame(&mut out, frame).unwrap();
    out
}

#[test]
fn a_query_frame_is_its_length_its_command_and_its_sql() {
    let frame = Request::Query("SELECT name FROM users WHERE id = 1".into()).encode();
    let bytes = bytes_of(&frame);
    assert_eq!(&bytes[..5], &[0x00, 0x00, 0x00, 0x24, 0x02]);
    assert_eq!(&bytes[5..], b"SELECT name FROM users WHERE id = 1");
}

#[test]
fn a_hello_frame_carries_the_magic_and_the_version() {
    let bytes = bytes_of(&Request::Hello { version: VERSION }.encode());
    assert_eq!(
        bytes,
        [0x00, 0x00, 0x00, 0x08, 0x01, b'O', b'A', b'K', b'D', b'B', 0x00, 0x01]
    );
}

#[test]
fn a_rows_frame_lists_the_columns_then_the_rows() {
    let response = Response::from(Output::Rows {
        columns: vec!["name".into()],
        rows: vec![vec![Value::Text("kim".into())]],
    });
    let bytes = bytes_of(&response.encode().unwrap());
    assert_eq!(
        bytes,
        [
            0x00, 0x00, 0x00, 0x15, 0x82, // length 21, ROWS
            0x00, 0x01, 0x00, 0x04, b'n', b'a', b'm', b'e', // one column, "name"
            0x00, 0x00, 0x00, 0x01, // one row
            0x02, 0x00, 0x00, 0x00, 0x03, b'k', b'i', b'm', // TEXT 'kim'
        ]
    );
}

#[test]
fn the_other_responses_have_their_own_commands() {
    assert_eq!(
        bytes_of(&Response::Affected(3).encode().unwrap()),
        [0, 0, 0, 9, command::AFFECTED, 0, 0, 0, 0, 0, 0, 0, 3]
    );
    assert_eq!(
        bytes_of(&Response::Done.encode().unwrap()),
        [0, 0, 0, 1, command::DONE]
    );
    assert_eq!(
        bytes_of(&Response::Ready { version: 1 }.encode().unwrap()),
        [0, 0, 0, 3, command::READY, 0, 1]
    );
    assert_eq!(
        bytes_of(
            &Response::Error {
                code: code::NO_SUCH_TABLE,
                message: "no".into()
            }
            .encode()
            .unwrap()
        ),
        [0, 0, 0, 4, command::ERROR, 0x03, b'n', b'o']
    );
}

#[test]
fn reading_stops_cleanly_at_the_end_of_the_stream() {
    let mut empty: &[u8] = &[];
    assert!(matches!(read_frame(&mut empty), Ok(None)));
}

#[test]
fn a_frame_cut_short_is_truncated() {
    let mut header_only: &[u8] = &[0, 0, 0, 5, 0x02, b'a'];
    assert!(matches!(
        read_frame(&mut header_only),
        Err(FrameError::Truncated)
    ));
    let mut half_header: &[u8] = &[0, 0];
    assert!(matches!(
        read_frame(&mut half_header),
        Err(FrameError::Truncated)
    ));
}

#[test]
fn a_frame_of_length_zero_is_empty() {
    let mut zero: &[u8] = &[0, 0, 0, 0];
    assert!(matches!(read_frame(&mut zero), Err(FrameError::Empty)));
}

#[test]
fn a_frame_over_the_limit_is_refused_before_its_body_is_read() {
    // "GET " read as a length: an HTTP client knocking.
    let mut http: &[u8] = b"GET / HTTP/1.1\r\n";
    assert!(matches!(
        read_frame(&mut http),
        Err(FrameError::TooLarge(0x4745_5420))
    ));
    let over = (MAX_FRAME_LEN + 1).to_be_bytes();
    let mut over: &[u8] = &over;
    assert!(matches!(
        read_frame(&mut over),
        Err(FrameError::TooLarge(_))
    ));
}

#[test]
fn a_frame_at_the_limit_is_read() {
    let mut bytes = MAX_FRAME_LEN.to_be_bytes().to_vec();
    bytes.push(command::QUERY);
    bytes.resize(4 + MAX_FRAME_LEN as usize, b' ');
    let frame = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
    assert_eq!(frame.payload.len(), MAX_FRAME_LEN as usize - 1);
}

#[test]
fn writing_a_frame_over_the_limit_fails() {
    let frame = Frame {
        command: command::QUERY,
        payload: vec![b' '; MAX_FRAME_LEN as usize],
    };
    assert!(write_frame(&mut Vec::new(), &frame).is_err());
}

#[test]
fn a_hello_with_another_magic_is_refused() {
    let frame = Frame {
        command: command::HELLO,
        payload: b"PGSQL\x00\x01".to_vec(),
    };
    assert!(matches!(Request::decode(frame), Err(DecodeError::BadMagic)));
}

#[test]
fn a_query_that_is_not_utf8_is_told_apart() {
    let frame = Frame {
        command: command::QUERY,
        payload: vec![0xff, 0xfe],
    };
    assert!(matches!(Request::decode(frame), Err(DecodeError::NotUtf8)));
}

#[test]
fn an_unknown_command_is_refused() {
    let frame = Frame {
        command: 0x7e,
        payload: vec![],
    };
    assert!(matches!(
        Request::decode(frame.clone()),
        Err(DecodeError::UnknownCommand(0x7e))
    ));
    assert!(matches!(
        Response::decode(frame),
        Err(DecodeError::UnknownCommand(0x7e))
    ));
}

#[test]
fn rows_with_no_columns_cannot_claim_rows() {
    // Each row would take no bytes, so a tiny frame could claim four
    // billion of them.
    let frame = Frame {
        command: command::ROWS,
        payload: vec![0, 0, 0xff, 0xff, 0xff, 0xff],
    };
    assert!(matches!(
        Response::decode(frame),
        Err(DecodeError::Malformed(_))
    ));
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Integer),
        ".{0,20}".prop_map(Value::Text),
        prop::collection::vec(any::<u8>(), 0..20).prop_map(Value::Blob),
    ]
}

fn response() -> impl Strategy<Value = Response> {
    let rows = (1usize..4).prop_flat_map(|width| {
        (
            prop::collection::vec("[a-z]{1,8}", width),
            prop::collection::vec(prop::collection::vec(value(), width), 0..5),
        )
    });
    prop_oneof![
        any::<u16>().prop_map(|version| Response::Ready { version }),
        rows.prop_map(|(columns, rows)| Response::Rows { columns, rows }),
        Just(Response::Rows {
            columns: vec![],
            rows: vec![]
        }),
        any::<u64>().prop_map(Response::Affected),
        Just(Response::Done),
        (any::<u8>(), ".{0,40}").prop_map(|(code, message)| Response::Error { code, message }),
    ]
}

proptest! {
    #[test]
    fn every_response_reads_back_as_written(response in response()) {
        let bytes = bytes_of(&response.encode().unwrap());
        let frame = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
        prop_assert_eq!(Response::decode(frame).unwrap(), response);
    }

    #[test]
    fn every_request_reads_back_as_written(sql in ".{0,60}", version in any::<u16>()) {
        for request in [Request::Query(sql.clone()), Request::Hello { version }] {
            let bytes = bytes_of(&request.encode());
            let frame = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            prop_assert_eq!(Request::decode(frame).unwrap(), request);
        }
    }

    #[test]
    fn random_bytes_never_make_the_decoder_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..64),
        command in prop_oneof![Just(command::ROWS), Just(command::ERROR), Just(command::READY),
                               Just(command::AFFECTED), Just(command::HELLO), any::<u8>()],
    ) {
        let frame = Frame { command, payload: bytes.clone() };
        let _ = Response::decode(frame.clone());
        let _ = Request::decode(frame);
        let _ = read_frame(&mut bytes.as_slice());
    }
}

#[test]
fn a_length_that_does_not_fit_its_field_is_an_error_not_a_cut() {
    let response = Response::Rows {
        columns: vec!["c".repeat(u16::MAX as usize + 1)],
        rows: vec![],
    };
    let err = response.encode().unwrap_err();
    assert!(err.message.contains("column name"), "{}", err.message);
}

#[test]
fn a_response_over_the_frame_limit_is_an_error() {
    let response = Response::Rows {
        columns: vec!["data".into()],
        rows: vec![vec![Value::Blob(vec![0; MAX_FRAME_LEN as usize])]],
    };
    assert!(response.encode().is_err());
}

#[test]
fn only_codes_that_break_the_conversation_close_the_connection() {
    for c in [
        code::MALFORMED,
        code::FRAME_TOO_LARGE,
        code::HANDSHAKE,
        code::TOO_MANY_CONNECTIONS,
        code::NOT_ALLOWED,
        code::IDLE_IN_TRANSACTION,
        code::IDLE,
        code::SHUTTING_DOWN,
        code::INTERNAL,
    ] {
        assert!(closes(c), "0x{c:02x}");
    }
    for c in [
        code::SYNTAX,
        code::TRANSACTION_STATE,
        code::CONFLICT,
        code::STOPPED,
        code::NOT_UTF8,
        code::RESULT_TOO_LARGE,
    ] {
        assert!(!closes(c), "0x{c:02x}");
    }
}

/// A conflict is code 0x20 ("run it again") whether a statement that ran on
/// its own lost it or the COMMIT of a transaction BEGIN opened did (PR #78
/// review S2).
#[test]
fn both_kinds_of_conflict_carry_the_conflict_code() {
    let conflict = || oakdb::Error::Conflict { key: vec![0] };
    assert_eq!(error_code(&SqlError::Engine(conflict())), code::CONFLICT);
    assert_eq!(
        error_code(&SqlError::TransactionConflict(conflict())),
        code::CONFLICT
    );
}

/// BEGIN twice, or COMMIT or ROLLBACK with nothing open, has a code of its
/// own (issue #37, design 0025 Q2); the session-only error a lone
/// statement gives stays a plain statement error.
#[test]
fn transaction_state_errors_carry_0x0b() {
    use oakdb_sql::TransactionError;
    for e in [TransactionError::AlreadyBegun, TransactionError::NotBegun] {
        assert_eq!(
            error_code(&SqlError::Transaction(e)),
            code::TRANSACTION_STATE
        );
    }
    assert_eq!(
        error_code(&SqlError::Transaction(TransactionError::NeedsSession)),
        code::OTHER_STATEMENT
    );
}
