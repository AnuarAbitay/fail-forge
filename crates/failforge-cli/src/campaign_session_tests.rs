use super::*;

fn clean() -> Value {
    json!({"originalUnchanged":true,"snapshotCleaned":true,"cleanupDeferred":false,
        "errors":[],"skipped":[],"workspaceDirectory":"/registry/campaign"})
}
fn complete(status: &str) -> Result<Completion, NamespaceError> {
    Ok(Completion {
        status: status.into(),
        error: None,
    })
}

#[test]
fn cleanup_and_original_integrity_override_success_and_cancellation() {
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    for (key, value) in [
        ("originalUnchanged", json!(false)),
        ("snapshotCleaned", json!(false)),
        ("cleanupDeferred", json!(true)),
        ("errors", json!(["cannot remove snapshot"])),
        ("errors", Value::Null),
    ] {
        let mut cleanup = clean();
        cleanup[key] = value;
        for result in [
            complete("completed"),
            Err(NamespaceError::Cancelled(libc::SIGTERM)),
        ] {
            let value = outcome(result, &cleanup, &cancel);
            assert_eq!(value.status, "failed", "{key}");
            assert!(
                value
                    .error
                    .unwrap()
                    .contains("cleanup or source verification")
            );
        }
    }
}

#[test]
fn failures_keep_precedence_over_late_cancellation_but_success_does_not() {
    let cancel = Cancellation::default();
    for status in ["completed", "no_mutants"] {
        let value = outcome(complete(status), &clean(), &cancel);
        assert_eq!(value.status, status);
        assert!(value.error.is_none());
    }
    cancel.request(libc::SIGTERM);
    for result in [
        complete("completed"),
        complete("no_mutants"),
        Err(NamespaceError::Cancelled(libc::SIGINT)),
    ] {
        let value = outcome(result, &clean(), &cancel);
        assert_eq!(value.status, "cancelled");
    }
    let failure = outcome(Err(invalid("changed dependency")), &clean(), &cancel);
    assert_eq!(failure.status, "failed");
    assert!(failure.error.unwrap().contains("changed dependency"));
    let failure = outcome(
        Ok(Completion {
            status: "failed".into(),
            error: Some("inner infrastructure failure".into()),
        }),
        &clean(),
        &cancel,
    );
    assert_eq!(failure.status, "failed");
    assert_eq!(
        failure.error.as_deref(),
        Some("inner infrastructure failure")
    );
}

#[test]
fn progress_failure_overrides_success_and_cancellation_but_preserves_execution_and_cleanup_errors()
{
    let cancel = Cancellation::default();
    cancel.request(libc::SIGINT);
    for result in [
        complete("completed"),
        complete("no_mutants"),
        complete("cancelled"),
        Err(NamespaceError::Cancelled(libc::SIGINT)),
        Err(NamespaceError::Cancelled(libc::SIGTERM)),
    ] {
        let completion = outcome(
            with_progress_failure(result, "progress write failed"),
            &clean(),
            &cancel,
        );
        assert_eq!(completion.status, "failed");
        assert_eq!(completion.error.as_deref(), Some("progress write failed"));
    }
    for result in [
        Err(invalid("SDK inputs changed")),
        Ok(Completion {
            status: "failed".into(),
            error: Some("SDK inputs changed".into()),
        }),
    ] {
        let completion = outcome(
            with_progress_failure(result, "progress write failed"),
            &clean(),
            &cancel,
        );
        assert_eq!(completion.status, "failed");
        assert_eq!(completion.error.as_deref(), Some("SDK inputs changed"));
    }
    let mut cleanup = clean();
    cleanup["errors"] = json!(["snapshot cleanup failed"]);
    let completion = outcome(
        with_progress_failure(Err(invalid("SDK inputs changed")), "progress write failed"),
        &cleanup,
        &cancel,
    );
    assert_eq!(completion.status, "failed");
    assert!(
        completion
            .error
            .unwrap()
            .contains("snapshot cleanup failed")
    );
}

#[tokio::test]
async fn blocking_operation_is_joined_and_failed_task_is_not_success() {
    let owner = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child = Arc::clone(&owner);
    let result = blocking(move || {
        child.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(41)
    })
    .await
    .unwrap();
    assert_eq!(result, 41);
    assert!(owner.load(std::sync::atomic::Ordering::SeqCst));
    let failure = blocking(|| -> Result<(), NamespaceError> { panic!("failed owner") }).await;
    assert!(
        failure
            .unwrap_err()
            .to_string()
            .contains("lifecycle operation failed")
    );
}

#[test]
fn progress_failure_cancels_new_work_without_losing_late_worker_events() {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "failforge-session-progress-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let mut progress = LiveProgress {
        state: ProgressState::new(
            "direct",
            "0123456789abcdef0123456789abcdef",
            &root,
            &["classic".into()],
        )
        .unwrap(),
        writer: ProgressWriter::new(&root, crate::campaign_publish_fs::walk(&root).unwrap())
            .unwrap(),
        started: Instant::now(),
        error: None,
    };
    progress
        .state
        .observe(&json!({"kind":"discovery","family":"classic",
        "value":{"candidates":[null,null]}}))
        .unwrap();
    progress
        .state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":2}))
        .unwrap();
    // A foreign entry appearing after binding must never be overwritten.
    fs::create_dir(root.join("progress.json")).unwrap();
    fs::write(root.join("progress.json/sentinel"), "foreign").unwrap();
    let cancel = Cancellation::default();
    let mut bytes = Vec::new();
    {
        let mut stream = Stream::new(&mut bytes);
        let mut sink = CampaignSink {
            stream: &mut stream,
            completion: None,
            progress: &mut progress,
            cancel: &cancel,
        };
        for id in ["first", "late"] {
            sink.emit_campaign(json!({"kind":"family_event","family":"classic",
                "event":{"kind":"mutant","record":{"recordKind":"mutation",
                    "family":"classic","mutantId":id,"outcome":"KILLED","reason":"test_failure"}}}))
                .unwrap();
        }
        sink.finish_campaign("cancelled", None, 0, 1, 2, Some(2))
            .unwrap();
    }
    assert_eq!(cancel.signum(), Some(libc::SIGINT));
    let failure = progress.error.clone().unwrap();
    assert!(failure.contains("campaign progress publication failed"));
    assert_eq!(
        progress.state.snapshot(0.0, None).unwrap()["completedMutants"],
        2
    );
    let events = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    for (index, id) in ["first", "late"].into_iter().enumerate() {
        assert_eq!(events[index]["event"]["event"]["record"]["mutantId"], id);
    }
    assert_eq!(
        fs::read_to_string(root.join("progress.json/sentinel")).unwrap(),
        "foreign"
    );
    fs::remove_dir_all(root.join("progress.json")).unwrap();
    // Latching the error prevents a later write from disguising the failure.
    progress.write(Some("failed"), &cancel);
    assert_eq!(progress.error.as_deref(), Some(failure.as_str()));
    assert!(!root.join("progress.json").exists());
    let result = outcome(Err(invalid(&failure)), &clean(), &cancel);
    assert_eq!(result.status, "failed");
    fs::remove_dir_all(root).unwrap();
}
