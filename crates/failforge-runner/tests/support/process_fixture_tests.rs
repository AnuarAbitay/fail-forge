use super::{
    decode_hex, hex, parse_args, parse_count, run_mode, watchdog_requests_exit, write_pattern,
    write_raw,
};
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

#[test]
fn every_u8_exit_code_is_accepted() {
    for code in u8::MIN..=u8::MAX {
        assert_eq!(
            parse_args(Some("exit".into()), Some(code.to_string().into()), None),
            Some(code),
            "code={code}"
        );
    }
}

#[test]
fn additional_numeric_forms_are_accepted() {
    for (text, expected) in [
        ("+0", 0),
        ("+37", 37),
        ("+255", 255),
        ("00", 0),
        ("00037", 37),
        ("000255", 255),
    ] {
        assert_eq!(
            parse_args(Some("exit".into()), Some(text.into()), None),
            Some(expected),
            "code={text:?}"
        );
    }
}

#[test]
fn argument_presence_matches_contract() {
    for has_mode in [false, true] {
        for has_code in [false, true] {
            for has_extra in [false, true] {
                let mode = has_mode.then(|| OsString::from("exit"));
                let code = has_code.then(|| OsString::from("37"));
                let extra = has_extra.then(|| OsString::from("extra"));
                let expected = if has_mode && has_code && !has_extra {
                    Some(37)
                } else {
                    None
                };

                assert_eq!(
                    parse_args(mode, code, extra),
                    expected,
                    "has_mode={has_mode}, has_code={has_code}, has_extra={has_extra}"
                );
            }
        }
    }
}

#[test]
fn any_extra_argument_is_rejected() {
    for extra in [
        OsString::new(),
        OsString::from("extra"),
        OsString::from(" "),
    ] {
        assert_eq!(
            parse_args(Some("exit".into()), Some("37".into()), Some(extra.clone())),
            None,
            "extra={extra:?}"
        );
    }

    #[cfg(unix)]
    for bytes in [b"\xff".as_slice(), b"\xfe".as_slice()] {
        let extra = OsString::from_vec(bytes.to_vec());
        assert_eq!(
            parse_args(Some("exit".into()), Some("37".into()), Some(extra)),
            None,
            "extra bytes={bytes:?}"
        );
    }
}

#[test]
fn unknown_modes_are_rejected() {
    for mode in [
        "",
        "other",
        "EXIT",
        "Exit",
        "exit ",
        " exit",
        "exit\n",
        "exit-suffix",
        "éxit",
        "exіt",
    ] {
        assert_eq!(
            parse_args(Some(mode.into()), Some("37".into()), None),
            None,
            "mode={mode:?}"
        );
    }
}

#[test]
fn invalid_exit_codes_are_rejected() {
    for code in [
        "",
        "abc",
        "-1",
        "-0",
        "-255",
        "256",
        "257",
        "65535",
        "18446744073709551616",
        "+",
        "++37",
        "+256",
        "0x25",
        "0b100101",
        "0o45",
        "37.0",
        "3_7",
        " 37",
        "37 ",
        "\t37",
        "37\n",
        "\u{00a0}37",
        "３７",
        "٣٧",
    ] {
        assert_eq!(
            parse_args(Some("exit".into()), Some(code.into()), None),
            None,
            "code={code:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_modes_are_rejected_without_panicking() {
    for bytes in [
        b"\xff".as_slice(),
        b"\xfe".as_slice(),
        b"exit\xff".as_slice(),
        b"ex\xfeit".as_slice(),
    ] {
        assert_eq!(
            parse_args(
                Some(OsString::from_vec(bytes.to_vec())),
                Some("37".into()),
                None,
            ),
            None,
            "mode bytes={bytes:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_exit_codes_are_rejected_without_panicking() {
    for bytes in [
        b"\xff".as_slice(),
        b"\xfe".as_slice(),
        b"37\xff".as_slice(),
        b"0\xfe".as_slice(),
        b"\xff37".as_slice(),
    ] {
        assert_eq!(
            parse_args(
                Some("exit".into()),
                Some(OsString::from_vec(bytes.to_vec())),
                None,
            ),
            None,
            "code bytes={bytes:?}"
        );
    }
}

#[test]
fn raw_hex_preserves_all_byte_values_and_empty_input() {
    assert_eq!(hex(&[]), "");
    let bytes: Vec<_> = (u8::MIN..=u8::MAX).collect();
    let encoded = hex(&bytes);
    assert_eq!(encoded.len(), 512);
    for (number, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        assert_eq!(
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap(),
            number as u8
        );
    }
}

#[test]
fn raw_decoder_accepts_empty_and_both_ascii_hex_cases() {
    for (encoded, expected) in [
        ("", b"".as_slice()),
        ("00", b"\0".as_slice()),
        ("Ff", b"\xff".as_slice()),
        ("00aAFf7e", b"\0\xaa\xff~".as_slice()),
    ] {
        assert_eq!(
            decode_hex(&OsString::from(encoded)).unwrap(),
            expected,
            "encoded={encoded:?}"
        );
    }
}

#[test]
fn raw_decoder_round_trips_every_byte_in_both_hex_cases() {
    let expected: Vec<_> = (u8::MIN..=u8::MAX).collect();
    let encoded = hex(&expected);
    for text in [encoded.clone(), encoded.to_ascii_uppercase()] {
        assert_eq!(decode_hex(&OsString::from(text)).unwrap(), expected);
    }
}

#[test]
fn raw_decoder_rejects_odd_lengths_and_non_hex_ascii() {
    for encoded in [
        "0", "abc", "00000", "0x", "x0", "0g", "g0", "GG", "+f", "-f", " f", "f ", "\t0", "0\n",
        "0_", "0x00", "\0\0",
    ] {
        assert_eq!(
            decode_hex(&OsString::from(encoded)).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput,
            "encoded={encoded:?}"
        );
    }
}

#[test]
fn raw_decoder_rejects_non_ascii_native_bytes_without_panicking() {
    for bytes in [
        b"\xff\xfe".as_slice(),
        b"0\xff".as_slice(),
        b"\xff0".as_slice(),
        "é".as_bytes(),
        "ＦＦ".as_bytes(),
    ] {
        assert_eq!(
            decode_hex(&OsString::from_vec(bytes.to_vec()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput,
            "native bytes={bytes:?}"
        );
    }
}

#[test]
fn raw_mode_validates_both_streams_before_starting_output() {
    for args in [["gg", "ff"], ["ff", "gg"], ["", "f"]] {
        let args: Vec<_> = args.into_iter().map(OsString::from).collect();
        assert_eq!(
            run_mode("raw", &args).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn raw_writer_preserves_binary_and_empty_output_through_partial_writes() {
    use std::io::{self, Write};
    #[derive(Default)]
    struct PartialWriter {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
    }
    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    for bytes in [Vec::new(), (u8::MIN..=u8::MAX).collect()] {
        let mut writer = PartialWriter::default();
        write_raw(&mut writer, &bytes).unwrap();
        assert_eq!(writer.bytes, bytes);
        assert_eq!(writer.flushes, 1);
        if bytes.is_empty() {
            assert_eq!(writer.writes, 0);
        } else {
            assert!(writer.writes > bytes.len() / 3);
        }
    }
}

#[test]
fn raw_writer_propagates_write_zero_write_error_and_flush_failure() {
    use std::io::{self, Write};
    struct Fails {
        write: Result<usize, io::ErrorKind>,
    }
    impl Write for Fails {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.write
                .map(|count| count.min(bytes.len()))
                .map_err(Into::into)
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    for (write, bytes, expected) in [
        (
            Err(io::ErrorKind::BrokenPipe),
            b"a".as_slice(),
            io::ErrorKind::BrokenPipe,
        ),
        (Ok(0), b"a".as_slice(), io::ErrorKind::WriteZero),
        (Ok(1), b"a".as_slice(), io::ErrorKind::PermissionDenied),
        (Ok(0), b"".as_slice(), io::ErrorKind::PermissionDenied),
    ] {
        assert_eq!(
            write_raw(Fails { write }, bytes).unwrap_err().kind(),
            expected
        );
    }
}

#[test]
fn byte_counts_accept_native_usize_range_and_reject_invalid_values() {
    for count in [0, 1, 16385, usize::MAX] {
        assert_eq!(
            parse_count(&OsString::from(count.to_string())).unwrap(),
            count
        );
    }
    for value in ["", "-1", "a", "1.5", " 1", "184467440737095516160"] {
        assert!(parse_count(&OsString::from(value)).is_err());
    }
    assert!(parse_count(&OsString::from_vec(vec![255])).is_err());
}

#[test]
fn output_pattern_is_seamless_across_block_boundaries() {
    for count in [0, 1, 5, 16384, 16385, 16386, 32773] {
        let mut bytes = Vec::new();
        write_pattern(&mut bytes, count).unwrap();
        assert_eq!(bytes.len(), count);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(*byte, [0, 255, b'a', 254, b'\n'][index % 5]);
        }
    }
}

#[test]
fn pattern_writer_propagates_partial_write_and_flush_failures() {
    use std::io::{self, Write};
    struct Fails {
        flush_only: bool,
    }
    impl Write for Fails {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.flush_only {
                Ok(bytes.len())
            } else {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    assert_eq!(
        write_pattern(Fails { flush_only: false }, 5)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        write_pattern(Fails { flush_only: true }, 5)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        write_pattern(Fails { flush_only: true }, 0)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn byte_watchdog_recognizes_only_the_explicit_cleanup_command() {
    for command in [b"exit 0\n".as_slice(), b"\texit\t0 \r\n".as_slice()] {
        assert!(watchdog_requests_exit(command));
    }
    for command in [
        b"".as_slice(),
        b"exit\n".as_slice(),
        b"exit 1\n".as_slice(),
        b"exit 0 extra\n".as_slice(),
        b"exit-suffix 0\n".as_slice(),
        b"EXIT 0\n".as_slice(),
        b"exit \xff\n".as_slice(),
    ] {
        assert!(!watchdog_requests_exit(command));
    }
}
