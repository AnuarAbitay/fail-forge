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
