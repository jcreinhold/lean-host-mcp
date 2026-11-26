//! Turn order, reuse, expiry and proxy turns, without Lean.

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
    let handle = tokio::runtime::Handle::current();
    let first = lock.try_read().ok_or("a free lock grants a read turn")?;
    lock.observe_demand(Demand::Present, &handle);
    assert!(lock.try_read().is_none(), "a queued writer holds back new readers");
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
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!later.is_finished(), "the later reader waits for the writer too");
    assert_eq!(finished(&lock), 0);
    lock.observe_demand(Demand::Absent, &handle);
    let granted = later.await?.map_err(|refused| format!("{refused:?}"))?;
    assert_eq!(granted, 1, "the next reader sees the finished write turn");
    Ok(())
}

#[tokio::test]
async fn a_proxy_turn_queued_before_its_demand_ended_still_finishes() -> TestResult {
    let lock = Arc::new(ProjectLock::default());
    let handle = tokio::runtime::Handle::current();
    let first = lock.try_read().ok_or("a free lock grants a read turn")?;
    lock.observe_demand(Demand::Present, &handle);
    lock.observe_demand(Demand::Absent, &handle);
    assert_eq!(lock.blocked_by().as_deref(), Some(EXTERNAL_WRITER));
    // While that turn waits, new demand queues no second proxy.
    lock.observe_demand(Demand::Present, &handle);
    assert_eq!(lock.writers.lock().line.len(), 1);
    drop(first);
    wait_for(|| finished(&lock) == 1).await?;
    wait_for(|| lock.try_read().is_some()).await?;
    assert_eq!(lock.blocked_by(), None);
    Ok(())
}

#[tokio::test]
async fn a_nested_read_reuses_the_turn_while_a_writer_waits() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let other = root.join("other");
    let lock = Arc::new(ProjectLock::default());
    let handle = tokio::runtime::Handle::current();
    let cancellation = CallCancellation::default();
    let until = deadline(WAIT)?;
    let (first, nested, elsewhere, seen) = scope(async {
        let first = ensure_read(&lock, &root, until, &cancellation).await;
        lock.observe_demand(Demand::Present, &handle);
        let nested = ensure_read(&lock, &root, until, &cancellation).await;
        let elsewhere = ensure_read(&lock, &other, until, &cancellation).await;
        (first, nested, elsewhere, (granted_at(&root), granted_at(&other)))
    })
    .await;
    assert!(matches!(first, Ok(0)), "{first:?}");
    assert!(
        matches!(nested, Ok(0)),
        "a nested read must not queue behind the writer: {nested:?}"
    );
    assert!(matches!(&elsewhere, Err(TurnRefused::OtherRoot(held)) if *held == root));
    assert_eq!(seen, (Some(0), None), "a request reads the count of its own turn");
    assert_eq!(granted_at(&root), None, "no turn outside a request");
    // The request ended, so the writer gets its turn.
    lock.observe_demand(Demand::Absent, &handle);
    wait_for(|| finished(&lock) == 1).await
}

#[tokio::test]
async fn a_read_wait_ends_at_its_deadline_or_on_cancel() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let lock = Arc::new(ProjectLock::default());
    let handle = tokio::runtime::Handle::current();
    lock.observe_demand(Demand::Present, &handle);
    let short = deadline(Duration::from_millis(30))?;
    let expired = scope(ensure_read(&lock, &root, short, &CallCancellation::default())).await;
    assert!(
        matches!(&expired, Err(TurnRefused::Expired { blocked_by: Some(name) }) if name == EXTERNAL_WRITER),
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
    lock.observe_demand(Demand::Absent, &handle);
    wait_for(|| finished(&lock) == 1).await
}
