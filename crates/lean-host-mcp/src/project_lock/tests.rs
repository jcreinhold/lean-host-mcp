//! Turn order, reuse, expiry and read shares, without Lean.

use std::time::Duration;

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(5);

fn deadline(after: Duration) -> Result<Instant, Box<dyn std::error::Error>> {
    Ok(Instant::now().checked_add(after).ok_or("deadline out of range")?)
}

async fn wait_for(mut condition: impl FnMut() -> bool) -> TestResult {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    Ok(())
}

fn finished(lock: &ProjectLock) -> u64 {
    lock.builds_finished()
}

#[tokio::test]
async fn a_queued_writer_goes_before_a_later_reader_which_sees_its_finish() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let first = lock.try_read().ok_or("a free lock grants a read turn")?;
    let build = spawn_write(&lock, "build-1", deadline(WAIT)?, CallCancellation::default());
    wait_for(|| line(&lock) == 1).await?;
    wait_for(|| lock.try_read().is_none()).await?;
    let later = {
        let lock = Arc::clone(&lock);
        let until = deadline(WAIT)?;
        tokio::spawn(scope(async move {
            ensure_read(&lock, &root, until, &CallCancellation::default()).await
        }))
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!later.is_finished());
    drop(first);
    finish_after(build.await?.map_err(|refused| format!("{refused:?}"))?, async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!later.is_finished(), "the later reader waits for the writer too");
        assert_eq!(finished(&lock), 0);
        Ok(())
    })
    .await?;
    let granted = later.await?.map_err(|refused| format!("{refused:?}"))?;
    assert_eq!(granted, 1, "the next reader sees the finished write turn");
    Ok(())
}

#[tokio::test]
async fn two_requests_hold_read_turns_at_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let both = Arc::new(tokio::sync::Barrier::new(2));
    let until = deadline(WAIT)?;
    let check = || {
        let (lock, root, both) = (Arc::clone(&lock), root.clone(), Arc::clone(&both));
        tokio::spawn(scope(async move {
            let granted = ensure_read(&lock, &root, until, &CallCancellation::default()).await;
            // Each request holds its turn here until the other has one too.
            let met = tokio::time::timeout(WAIT, both.wait()).await.is_ok();
            (granted, met)
        }))
    };
    for task in [check(), check()] {
        let (granted, met) = task.await?;
        assert!(matches!(granted, Ok(0)), "{granted:?}");
        assert!(met, "a read turn waited for another read turn");
    }
    Ok(())
}

#[tokio::test]
async fn a_read_share_keeps_a_build_waiting_after_its_request_ends() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let until = deadline(WAIT)?;
    let cancellation = CallCancellation::default();
    let (outside, share, elsewhere) = scope(async {
        let outside = read_share(&root).await;
        ensure_read(&lock, &root, until, &cancellation)
            .await
            .map_err(|refused| format!("{refused:?}"))?;
        Ok::<_, String>((outside, read_share(&root).await, read_share(&root.join("other")).await))
    })
    .await?;
    assert!(outside.is_none(), "no share before the request holds a turn");
    assert!(elsewhere.is_none(), "no share of a turn on another root");
    let share = share.ok_or("a request with a turn gives a share")?;
    assert!(read_share(&root).await.is_none(), "no share outside a request");
    // The request ended, but a short run still holds its share.
    let build_cancellation = CallCancellation::default();
    let build = ensure_write(&lock, "build-1", deadline(WAIT)?, &build_cancellation);
    tokio::pin!(build);
    tokio::select! {
        turn = &mut build => {
            let refused = turn.err();
            return Err(format!("a build's wait ended while a share was held: {refused:?}").into());
        }
        () = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    drop(share);
    build.await.map_err(|refused| format!("{refused:?}"))?.finish();
    assert_eq!(finished(&lock), 1);
    Ok(())
}

#[tokio::test]
async fn a_nested_read_reuses_the_turn_while_a_writer_waits() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let other = root.join("other");
    let lock = Arc::new(ProjectLock::default());
    let cancellation = CallCancellation::default();
    let until = deadline(WAIT)?;
    let (first, build, nested, elsewhere, seen) = scope(async {
        let first = ensure_read(&lock, &root, until, &cancellation).await;
        let build = spawn_write(&lock, "build-1", until, CallCancellation::default());
        wait_for(|| line(&lock) == 1).await?;
        let nested = ensure_read(&lock, &root, until, &cancellation).await;
        let elsewhere = ensure_read(&lock, &other, until, &cancellation).await;
        Ok::<_, Box<dyn std::error::Error>>((first, build, nested, elsewhere, (granted_at(&root), granted_at(&other))))
    })
    .await?;
    assert!(matches!(first, Ok(0)), "{first:?}");
    assert!(
        matches!(nested, Ok(0)),
        "a nested read must not queue behind the writer: {nested:?}"
    );
    assert!(matches!(&elsewhere, Err(TurnRefused::OtherRoot(held)) if *held == root));
    assert_eq!(seen, (Some(0), None), "a request reads the count of its own turn");
    assert_eq!(granted_at(&root), None, "no turn outside a request");
    // The request ended, so the writer gets its turn.
    build.await?.map_err(|refused| format!("{refused:?}"))?.finish();
    assert_eq!(finished(&lock), 1);
    Ok(())
}

#[tokio::test]
async fn a_read_wait_ends_at_its_deadline_or_on_cancel() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let build = spawn_write(&lock, "build-1", deadline(WAIT)?, CallCancellation::default());
    finish_after(build.await?.map_err(|refused| format!("{refused:?}"))?, async {
        let short = deadline(Duration::from_millis(30))?;
        let expired = scope(ensure_read(&lock, &root, short, &CallCancellation::default())).await;
        assert!(
            matches!(&expired, Err(TurnRefused::Expired { blocked_by: Some(name) }) if name == "build-1"),
            "{expired:?}"
        );
        let cancellation = CallCancellation::default();
        let waiting = scope(ensure_read(&lock, &root, deadline(WAIT)?, &cancellation));
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => return Err(format!("read granted during a write turn: {result:?}").into()),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        cancellation.cancel();
        let cancelled = waiting.await;
        assert!(matches!(cancelled, Err(TurnRefused::Cancelled)), "{cancelled:?}");
        Ok(())
    })
    .await?;
    assert_eq!(finished(&lock), 1);
    Ok(())
}

fn line(lock: &ProjectLock) -> usize {
    lock.writers.lock().line.len()
}

fn spawn_write(
    lock: &Arc<ProjectLock>,
    name: &'static str,
    until: Instant,
    cancellation: CallCancellation,
) -> tokio::task::JoinHandle<Result<WriteTurn, TurnRefused>> {
    let lock = Arc::clone(lock);
    tokio::spawn(async move { ensure_write(&lock, name, until, &cancellation).await })
}

/// Hold `turn` while `checks` run, then finish it.
async fn finish_after(turn: WriteTurn, checks: impl Future<Output = TestResult>) -> TestResult {
    let result = checks.await;
    turn.finish();
    result
}

#[tokio::test]
async fn builds_and_checks_take_turns_in_arrival_order() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let first = lock.try_read().ok_or("a free lock grants a read turn")?;
    let one = spawn_write(&lock, "build-1", deadline(WAIT)?, CallCancellation::default());
    wait_for(|| line(&lock) == 1).await?;
    let two = spawn_write(&lock, "build-2", deadline(WAIT)?, CallCancellation::default());
    wait_for(|| line(&lock) == 2).await?;
    let reader = {
        let lock = Arc::clone(&lock);
        let until = deadline(WAIT)?;
        tokio::spawn(scope(async move {
            ensure_read(&lock, &root, until, &CallCancellation::default()).await
        }))
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(lock.blocked_by().as_deref(), Some("build-1"));
    drop(first);
    finish_after(one.await?.map_err(|refused| format!("{refused:?}"))?, async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!two.is_finished(), "the second build waits for the first");
        assert!(!reader.is_finished(), "the check waits for both builds");
        Ok(())
    })
    .await?;
    finish_after(two.await?.map_err(|refused| format!("{refused:?}"))?, async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!reader.is_finished(), "a build that came first goes first");
        assert_eq!(lock.blocked_by().as_deref(), Some("build-2"));
        Ok(())
    })
    .await?;
    let granted = reader.await?.map_err(|refused| format!("{refused:?}"))?;
    assert_eq!(granted, 2, "the check sees both finished builds");
    assert_eq!(line(&lock), 0);
    Ok(())
}

#[tokio::test]
async fn a_write_wait_ends_at_its_deadline_or_on_cancel_and_leaves_the_line() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    // A check holds the turn and no writer is ahead: the expiry names nobody.
    let check = lock.try_read().ok_or("a free lock grants a read turn")?;
    let short = deadline(Duration::from_millis(30))?;
    let expired = spawn_write(&lock, "build-0", short, CallCancellation::default())
        .await?
        .err();
    assert!(
        matches!(&expired, Some(TurnRefused::Expired { blocked_by: None })),
        "{expired:?}"
    );
    assert_eq!(line(&lock), 0, "an expired build leaves the line");
    drop(check);
    // A build holds the turn: a later build and a later check name it.
    let held = spawn_write(&lock, "build-1", deadline(WAIT)?, CallCancellation::default())
        .await?
        .map_err(|refused| format!("{refused:?}"))?;
    let short = deadline(Duration::from_millis(30))?;
    let expired = spawn_write(&lock, "build-2", short, CallCancellation::default())
        .await?
        .err();
    assert!(
        matches!(&expired, Some(TurnRefused::Expired { blocked_by: Some(name) }) if name == "build-1"),
        "{expired:?}"
    );
    let short = deadline(Duration::from_millis(30))?;
    let refused = scope(ensure_read(&lock, &root, short, &CallCancellation::default())).await;
    assert!(
        matches!(&refused, Err(TurnRefused::Expired { blocked_by: Some(name) }) if name == "build-1"),
        "{refused:?}"
    );
    let cancellation = CallCancellation::default();
    let waiting = spawn_write(&lock, "build-3", deadline(WAIT)?, cancellation.clone());
    wait_for(|| line(&lock) == 2).await?;
    cancellation.cancel();
    let cancelled = waiting.await?.err();
    assert!(matches!(&cancelled, Some(TurnRefused::Cancelled)), "{cancelled:?}");
    assert_eq!(line(&lock), 1, "a cancelled build leaves the line");
    // A turn dropped without `finish` counts no build.
    drop(held);
    assert_eq!(line(&lock), 0);
    assert_eq!(finished(&lock), 0);
    Ok(())
}

#[tokio::test]
async fn a_check_cannot_build() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let until = deadline(WAIT)?;
    let cancellation = CallCancellation::default();
    let refused = scope(async {
        ensure_read(&lock, &root, until, &cancellation)
            .await
            .map_err(|refused| format!("{refused:?}"))?;
        Ok::<_, String>(ensure_write(&lock, "build-1", until, &cancellation).await.err())
    })
    .await?;
    assert!(matches!(refused, Some(TurnRefused::HoldsRead)), "{refused:?}");
    assert_eq!(line(&lock), 0);
    let build = spawn_write(&lock, "build-2", deadline(WAIT)?, CallCancellation::default());
    finish_after(build.await?.map_err(|refused| format!("{refused:?}"))?, async {
        assert_eq!(lock.blocked_by().as_deref(), Some("build-2"));
        Ok(())
    })
    .await?;
    assert_eq!(finished(&lock), 1);
    Ok(())
}
