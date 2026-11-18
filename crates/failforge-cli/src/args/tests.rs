use super::*;
use crate::test_support::absolute;

fn spec(values: &[&str]) -> Result<CommandSpec, clap::Error> {
    let cli = Cli::try_parse_from(values)?;
    let Commands::Run(run) = cli.command else {
        panic!("expected run subcommand");
    };
    run.into_spec(&absolute("/invocation"))
}

#[test]
fn clap_definition_is_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn defaults_match_runner_contract() {
    let command = spec(&["failforge", "run", "--", "tool"]).unwrap();
    assert_eq!(command.program, "tool");
    assert!(command.args.is_empty());
    assert_eq!(command.cwd, absolute("/invocation"));
    assert!(!command.env.clear);
    assert_eq!(command.timeout_policy.timeout, None);
    assert_eq!(command.timeout_policy.grace_period, Duration::from_secs(2));
    assert_eq!(command.timeout_policy.forced_wait, Duration::from_secs(5));
    assert_eq!(command.timeout_policy.output_drain, Duration::from_secs(1));
    assert_eq!(command.capture_policy.mode, OutputMode::Capture);
    assert_eq!(command.capture_policy.capture_dir, None);
}

#[test]
fn requires_run_program_and_separator() {
    for values in [
        vec!["failforge"],
        vec!["failforge", "run"],
        vec!["failforge", "run", "--"],
        vec!["failforge", "run", "tool"],
        vec!["failforge", "unknown"],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn child_arguments_are_preserved_without_shell_interpretation() {
    let command = spec(&[
        "failforge",
        "run",
        "--",
        "tool",
        "a b",
        "",
        "$(touch x)",
        "*",
        "--stream",
        "--",
    ])
    .unwrap();
    assert_eq!(
        command.args,
        ["a b", "", "$(touch x)", "*", "--stream", "--"].map(OsString::from)
    );
    assert_eq!(command.capture_policy.mode, OutputMode::Capture);
}

#[test]
fn expanded_options_build_the_spec() {
    let command = spec(&[
        "failforge",
        "run",
        "--timeout",
        "30s",
        "--cwd",
        "example",
        "--stream",
        "--capture-dir",
        "artifacts/run-001",
        "--env",
        "CI=true",
        "--env-remove",
        "DEBUG_TOKEN",
        "--grace-period",
        "0ms",
        "--",
        "flutter",
        "test",
    ])
    .unwrap();
    assert_eq!(command.cwd, absolute("/invocation").join("example"));
    assert_eq!(
        command.timeout_policy.timeout,
        Some(Duration::from_secs(30))
    );
    assert_eq!(command.timeout_policy.grace_period, Duration::ZERO);
    assert_eq!(command.capture_policy.mode, OutputMode::StreamAndCapture);
    assert_eq!(
        command.capture_policy.capture_dir,
        Some(absolute("/invocation").join("artifacts").join("run-001"))
    );
    assert_eq!(
        command.env.set.get(std::ffi::OsStr::new("CI")),
        Some(&OsString::from("true"))
    );
    assert!(
        command
            .env
            .remove
            .contains(std::ffi::OsStr::new("DEBUG_TOKEN"))
    );
}

#[test]
fn absolute_paths_override_invocation_directory() {
    let child = absolute("/child");
    let capture = absolute("/capture");
    let command = spec(&[
        "failforge",
        "run",
        "--cwd",
        child.to_str().unwrap(),
        "--capture-dir",
        capture.to_str().unwrap(),
        "--",
        "tool",
    ])
    .unwrap();
    assert_eq!(command.cwd, child);
    assert_eq!(command.capture_policy.capture_dir, Some(capture));
}

#[test]
fn assignments_split_at_first_equals_and_last_assignment_wins() {
    let command = spec(&[
        "failforge",
        "run",
        "--env",
        "TOKEN=old",
        "--env",
        "TOKEN=a=b=c",
        "--env",
        "EMPTY=",
        "--",
        "tool",
    ])
    .unwrap();
    assert_eq!(
        command.env.set.get(std::ffi::OsStr::new("TOKEN")),
        Some(&OsString::from("a=b=c"))
    );
    assert_eq!(
        command.env.set.get(std::ffi::OsStr::new("EMPTY")),
        Some(&OsString::new())
    );
}

#[test]
fn invalid_environment_is_rejected_without_echoing_secrets() {
    for extra in [
        vec!["--env", "secret_without_equals"],
        vec!["--env", "=secret"],
        vec!["--env", "TOKEN=secret", "--env-remove", "TOKEN"],
        vec!["--env-remove", "BAD=secret"],
    ] {
        let mut values = vec!["failforge", "run"];
        values.extend(extra);
        values.extend(["--", "tool"]);
        let error = match spec(&values) {
            Err(error) => error,
            Ok(_) => panic!("invalid environment accepted"),
        };
        assert!(!error.to_string().contains("secret"));
        assert_eq!(error.exit_code(), 2);
    }
}

#[test]
fn parser_diagnostic_redacts_the_raw_clap_value_error() {
    let marker = "PRIVATE_PARSE_SECRET";
    let error = match Cli::try_parse_from(["failforge", "run", "--timeout", marker, "--", "tool"]) {
        Err(error) => error,
        Ok(_) => panic!("invalid timeout accepted"),
    };
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains(marker));
    let diagnostic = crate::parse_error_diagnostic(&error).unwrap();
    assert!(!diagnostic.contains(marker));
    assert!(diagnostic.contains("invalid value"));
}

#[test]
fn every_input_error_category_omits_the_original_error_context() {
    for kind in [
        ErrorKind::InvalidValue,
        ErrorKind::UnknownArgument,
        ErrorKind::InvalidSubcommand,
        ErrorKind::NoEquals,
        ErrorKind::ValueValidation,
        ErrorKind::TooManyValues,
        ErrorKind::TooFewValues,
        ErrorKind::WrongNumberOfValues,
        ErrorKind::ArgumentConflict,
        ErrorKind::MissingRequiredArgument,
        ErrorKind::MissingSubcommand,
        ErrorKind::InvalidUtf8,
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        ErrorKind::Io,
        ErrorKind::Format,
    ] {
        let error = clap::Error::raw(kind, "PRIVATE_PARSE_SECRET");
        let diagnostic = crate::parse_error_diagnostic(&error).unwrap();
        assert!(!diagnostic.contains("PRIVATE_PARSE_SECRET"), "{kind:?}");
        assert!(diagnostic.starts_with("failforge: "), "{kind:?}");
        assert!(diagnostic.contains("--help"), "{kind:?}");
    }
}

#[test]
fn clear_env_requires_path_or_explicit_executable_path() {
    assert!(spec(&["failforge", "run", "--clear-env", "--", "tool"]).is_err());
    let with_path = spec(&[
        "failforge",
        "run",
        "--clear-env",
        "--env",
        "PATH=",
        "--",
        "tool",
    ])
    .unwrap();
    assert!(with_path.env.clear);
    let explicit = spec(&["failforge", "run", "--clear-env", "--", "./tool"]).unwrap();
    assert!(explicit.env.clear);
}

#[test]
fn duration_units_and_zero_grace_are_accepted() {
    for (text, expected) in [
        ("0ms", Duration::ZERO),
        ("1ms", Duration::from_millis(1)),
        ("30s", Duration::from_secs(30)),
        ("2m", Duration::from_secs(120)),
        ("0001s", Duration::from_secs(1)),
    ] {
        assert_eq!(parse_duration(text), Ok(expected));
    }
}

#[test]
fn durations_reject_invalid_numbers_units_and_overflow() {
    for text in [
        "",
        "1",
        "ms",
        "-1s",
        "+1s",
        "1.5s",
        "1 s",
        " 1s",
        "1s ",
        "1h",
        "1S",
        "１２s",
        "18446744073709551616ms",
        "18446744073709551615m",
    ] {
        assert!(parse_duration(text).is_err(), "accepted {text}");
    }
    assert_eq!(
        parse_duration("18446744073709551615ms"),
        Ok(Duration::from_millis(u64::MAX))
    );
    assert_eq!(
        parse_duration("18446744073709551615s"),
        Ok(Duration::from_secs(u64::MAX))
    );
    let max_minutes = u64::MAX / 60;
    assert_eq!(
        parse_duration(&format!("{max_minutes}m")),
        Ok(Duration::from_secs(max_minutes * 60))
    );
    assert!(parse_duration(&format!("{}m", max_minutes + 1)).is_err());
}

#[test]
fn timeout_must_be_positive_in_every_unit() {
    for text in ["0ms", "0s", "0m", "000ms", "-1s", "18446744073709551616s"] {
        assert!(spec(&["failforge", "run", "--timeout", text, "--", "tool"]).is_err());
    }
    assert_eq!(parse_timeout("1ms"), Ok(Duration::from_millis(1)));
}

#[test]
fn native_arguments_paths_and_environment_are_preserved() {
    let raw = crate::test_support::raw;
    let values = vec![
        raw(b"failforge"),
        raw(b"run"),
        raw(b"--cwd"),
        raw(b"child-\xff"),
        raw(b"--capture-dir"),
        raw(b"capture-\xfe"),
        raw(b"--env"),
        raw(b"KEY-\xfd=value-\xfc=tail"),
        raw(b"--env-remove"),
        raw(b"OLD-\xfb"),
        raw(b"--"),
        raw(b"./tool-\xfa"),
        raw(b"arg-\xf9"),
    ];
    let cli = Cli::try_parse_from(values).unwrap();
    let Commands::Run(run) = cli.command else {
        panic!("expected run subcommand");
    };
    let invocation = absolute("/invocation");
    let command = run.into_spec(&invocation).unwrap();
    assert_eq!(command.program, raw(b"./tool-\xfa"));
    assert_eq!(command.args, [raw(b"arg-\xf9")]);
    assert_eq!(command.cwd, invocation.join(raw(b"child-\xff")));
    assert_eq!(
        command.capture_policy.capture_dir.unwrap(),
        invocation.join(raw(b"capture-\xfe"))
    );
    assert_eq!(
        command.env.set.get(raw(b"KEY-\xfd").as_os_str()),
        Some(&raw(b"value-\xfc=tail"))
    );
    assert!(command.env.remove.contains(raw(b"OLD-\xfb").as_os_str()));
}

#[test]
fn invalid_native_configuration_does_not_echo_raw_arguments() {
    for option in ["--env", "--env-remove"] {
        let cli = Cli::try_parse_from([
            OsString::from("failforge"),
            OsString::from("run"),
            OsString::from(option),
            crate::test_support::raw(b"secret\0=value"),
            OsString::from("--"),
            OsString::from("tool"),
        ])
        .unwrap();
        let Commands::Run(run) = cli.command else {
            panic!("expected run subcommand");
        };
        let error = match run.into_spec(&absolute("/invocation")) {
            Err(error) => error,
            Ok(_) => panic!("invalid native environment accepted"),
        };
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn relative_invocation_directory_is_rejected() {
    let cli = Cli::try_parse_from(["failforge", "run", "--", "tool"]).unwrap();
    let Commands::Run(run) = cli.command else {
        panic!("expected run subcommand");
    };
    assert!(run.into_spec(Path::new("relative")).is_err());
}

#[test]
fn empty_program_and_missing_program_are_rejected_during_conversion() {
    let cli = Cli::try_parse_from(["failforge", "run", "--", ""]).unwrap();
    let Commands::Run(run) = cli.command else {
        panic!("expected run subcommand");
    };
    assert!(run.into_spec(Path::new("/invocation")).is_err());
    let cli = Cli::try_parse_from(["failforge", "run", "--", "tool"]).unwrap();
    let Commands::Run(mut run) = cli.command else {
        panic!("expected run subcommand");
    };
    run.command.clear();
    assert!(run.into_spec(Path::new("/invocation")).is_err());
}

#[test]
fn help_and_version_have_successful_clap_exit_codes() {
    for values in [
        vec!["failforge", "--help"],
        vec!["failforge", "--version"],
        vec!["failforge", "run", "--help"],
    ] {
        let error = match Cli::try_parse_from(values) {
            Err(error) => error,
            Ok(_) => panic!("help unexpectedly returned a command"),
        };
        assert_eq!(error.exit_code(), 0);
        assert!(crate::parse_error_diagnostic(&error).is_none());
    }
}
