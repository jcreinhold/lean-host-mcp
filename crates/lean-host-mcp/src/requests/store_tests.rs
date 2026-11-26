use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use anyhow::Context as _;

use super::{DATABASE_NAME, End, Insert, Kind, NewRow, RequestCounts, State, Store, StoreError};

fn new_row(id: &str, intent: &str, created_at: i64, log: Option<&str>) -> NewRow {
    NewRow {
        id: id.to_owned(),
        client: Some("test 1".to_owned()),
        tool: "lean_build".to_owned(),
        intent: intent.to_owned(),
        arguments: "{}".to_owned(),
        root: "/project".to_owned(),
        kind: Kind::Build,
        created_at,
        deadline_at: created_at.saturating_add(1000),
        log_path: log.map(str::to_owned),
    }
}

fn end(state: State, finished_at: i64) -> End {
    End {
        state,
        finished_at,
        exit_code: Some(0),
        result: Some("{\"content\":[]}".to_owned()),
    }
}

/// Insert a row and end it at `finished_at`.
async fn ended(store: &Store, id: &str, finished_at: i64, log: Option<&str>) -> anyhow::Result<()> {
    store.insert_or_get(new_row(id, "a", finished_at, log)).await?;
    anyhow::ensure!(store.finish(id.to_owned(), end(State::Done, finished_at)).await?);
    Ok(())
}

async fn ids(store: &Store) -> anyhow::Result<Vec<String>> {
    let conn = Arc::clone(&store.conn);
    let ids = tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<String>> {
        conn.lock()
            .prepare("SELECT id FROM requests ORDER BY id")?
            .query_map([], |row| row.get(0))?
            .collect()
    })
    .await??;
    Ok(ids)
}

#[tokio::test]
async fn one_id_holds_one_call() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let Insert::Inserted(row) = store.insert_or_get(new_row("req-0001", "a", 10, None)).await? else {
        anyhow::bail!("the first insert did not insert");
    };
    assert_eq!(row.state, State::Queued);
    assert_eq!(store.get("req-0001".to_owned()).await?, Some(row.clone()));
    assert_eq!(
        store.insert_or_get(new_row("req-0001", "a", 20, None)).await?,
        Insert::Existing(row.clone())
    );
    assert_eq!(
        store.insert_or_get(new_row("req-0001", "b", 30, None)).await?,
        Insert::Conflict(row)
    );
    assert_eq!(ids(&store).await?, ["req-0001"]);
    Ok(())
}

#[tokio::test]
async fn parallel_inserts_of_one_id_insert_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let mut tasks = Vec::new();
    for at in 0..16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store.insert_or_get(new_row("req-0001", "a", at, None)).await
        }));
    }
    let mut inserted = 0_u32;
    for task in tasks {
        if matches!(task.await??, Insert::Inserted(_)) {
            inserted = inserted.saturating_add(1);
        }
    }
    assert_eq!(inserted, 1);
    Ok(())
}

#[tokio::test]
async fn only_a_queued_row_starts_and_only_an_unended_row_ends() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.insert_or_get(new_row("req-0001", "a", 10, None)).await?;
    assert!(store.mark_running("req-0001".to_owned(), 11).await?);
    assert!(!store.mark_running("req-0001".to_owned(), 12).await?);
    assert!(!store.mark_running_blocking("req-0001", 12)?);
    let row = store.get("req-0001".to_owned()).await?.context("row")?;
    assert_eq!((row.state, row.started_at), (State::Running, Some(11)));

    assert!(store.finish("req-0001".to_owned(), end(State::Done, 13)).await?);
    assert!(!store.finish("req-0001".to_owned(), end(State::Interrupted, 14)).await?);
    assert!(!store.mark_running("req-0001".to_owned(), 15).await?);
    let row = store.get("req-0001".to_owned()).await?.context("row")?;
    assert_eq!(
        (row.state, row.finished_at, row.exit_code),
        (State::Done, Some(13), Some(0))
    );
    assert!(row.result.is_some());

    // A queued row may end without starting.
    store.insert_or_get(new_row("req-0002", "a", 10, None)).await?;
    assert!(store.finish("req-0002".to_owned(), end(State::TimedOut, 16)).await?);
    assert!(!store.mark_running("req-0002".to_owned(), 17).await?);
    Ok(())
}

#[tokio::test]
async fn a_final_row_never_changes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    ended(&store, "req-0001", 10, None).await?;
    let changed = store
        .conn
        .lock()
        .execute("UPDATE requests SET result='other' WHERE id='req-0001'", []);
    let error = changed.err().context("the trigger let a final row change")?;
    assert!(error.to_string().contains("final row"), "{error}");
    Ok(())
}

#[tokio::test]
async fn finish_refuses_a_state_that_is_not_final() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.insert_or_get(new_row("req-0001", "a", 10, None)).await?;
    let refused = store.finish("req-0001".to_owned(), end(State::Running, 11)).await;
    assert!(
        matches!(refused, Err(StoreError::NotFinal(State::Running))),
        "{refused:?}"
    );
    Ok(())
}

#[tokio::test]
async fn prune_drops_old_rows_then_the_oldest_past_the_cap_and_keeps_unended_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    ended(&store, "req-old1", 10, Some("/logs/old1.log")).await?;
    ended(&store, "req-old2", 20, None).await?;
    ended(&store, "req-new1", 100, Some("/logs/new1.log")).await?;
    ended(&store, "req-new2", 200, Some("/logs/new2.log")).await?;
    ended(&store, "req-new3", 300, None).await?;
    store
        .insert_or_get(new_row("req-open", "a", 0, Some("/logs/open.log")))
        .await?;
    store.insert_or_get(new_row("req-runs", "a", 0, None)).await?;
    store.mark_running("req-runs".to_owned(), 1).await?;

    // Before 50: the two old rows go. Cap 4: then the oldest ended row goes.
    let mut logs = store.prune(50, 4).await?;
    logs.sort();
    assert_eq!(
        logs,
        [std::path::PathBuf::from("/logs/new1.log"), "/logs/old1.log".into()]
    );
    assert_eq!(ids(&store).await?, ["req-new2", "req-new3", "req-open", "req-runs"]);

    // A cap below the unended rows leaves those rows.
    store.prune(0, 1).await?;
    assert_eq!(ids(&store).await?, ["req-open", "req-runs"]);
    let named: Vec<_> = store.log_paths().await?.into_iter().collect();
    assert_eq!(named, [std::path::PathBuf::from("/logs/open.log")]);
    Ok(())
}

#[tokio::test]
async fn counts_and_unfinished_see_only_unended_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.insert_or_get(new_row("req-0003", "a", 30, None)).await?;
    store.insert_or_get(new_row("req-0001", "a", 10, None)).await?;
    store.insert_or_get(new_row("req-0002", "a", 20, None)).await?;
    store.mark_running("req-0002".to_owned(), 21).await?;
    ended(&store, "req-0004", 5, None).await?;
    assert_eq!(store.counts().await?, RequestCounts { queued: 2, running: 1 });
    let unfinished: Vec<String> = store.unfinished().await?.into_iter().map(|row| row.id).collect();
    assert_eq!(unfinished, ["req-0001", "req-0002", "req-0003"]);
    Ok(())
}

#[tokio::test]
async fn open_uses_a_private_synced_write_ahead_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.insert_or_get(new_row("req-0001", "a", 10, None)).await?;
    let text = |pragma: &str| {
        store
            .conn
            .lock()
            .pragma_query_value(None, pragma, |row| row.get::<_, String>(0))
    };
    let number = |pragma: &str| {
        store
            .conn
            .lock()
            .pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
    };
    assert_eq!(text("journal_mode")?, "wal");
    assert_eq!(number("synchronous")?, 2, "synchronous must be FULL");
    assert_eq!(number("user_version")?, 1);
    if cfg!(target_os = "macos") {
        for pragma in ["fullfsync", "checkpoint_fullfsync"] {
            assert_eq!(number(pragma)?, 1, "{pragma}");
        }
    }
    for name in [DATABASE_NAME.to_owned(), format!("{DATABASE_NAME}-wal")] {
        let mode = std::fs::metadata(dir.path().join(&name))?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn a_reopened_table_keeps_its_rows_and_refuses_a_newer_schema() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.insert_or_get(new_row("req-0001", "a", 10, None)).await?;
    drop(store);
    let store = Store::open(dir.path())?;
    assert!(store.get("req-0001".to_owned()).await?.is_some());
    store.conn.lock().pragma_update(None, "user_version", 2)?;
    drop(store);
    let refused = Store::open(dir.path());
    assert!(matches!(refused, Err(StoreError::Newer(2))), "{refused:?}");
    Ok(())
}
