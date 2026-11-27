//! Recovery at start, before the server listens: end the rows an earlier
//! server left, then start the queued ones.
//!
//! A row that says `running` belonged to an earlier server. Its writer helper
//! may still run after that server died, so recovery takes the project's write
//! turn and opens a writer with no step: the helper gets the kernel lock only
//! once every earlier holder has exited. Then the row becomes `interrupted`;
//! it never runs again. A `queued` row never started, so recovery starts it in
//! `created_at` order, unless its deadline has passed (`timed_out`) or its
//! project is gone (`interrupted`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde_json::{Map, Value};

use super::{RequestRunner, State, now_millis};
use crate::server::ToolCall;

/// How long recovery waits for the helpers of an earlier server to exit.
const RECOVERY_WAIT: Duration = Duration::from_mins(15);

impl RequestRunner {
    /// Prune the table, end the rows an earlier server left running, and start
    /// the queued ones.
    ///
    /// # Errors
    /// Fails when the table cannot be read or written, or when an earlier
    /// server's helper keeps a project's kernel lock past the recovery wait.
    pub async fn recover(self: &Arc<Self>) -> anyhow::Result<()> {
        self.prune(true).await;
        let rows = self.store.unfinished().await.context("cannot read the request table")?;
        let mut running: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for row in rows.iter().filter(|row| row.state == State::Running) {
            running
                .entry(PathBuf::from(&row.root))
                .or_default()
                .push(row.id.clone());
        }
        if !running.is_empty() {
            let helper = lean_rs_writer::contract()
                .context("cannot find the writer helper")?
                .executable;
            for (root, ids) in running {
                if root.is_dir() {
                    let started = Instant::now();
                    tracing::info!(root = %root.display(), requests = ids.len(), "waiting for the helpers of an earlier server to exit");
                    crate::build::confirm_no_writer(
                        &self.ctx.broker.project_lock(&root),
                        &root,
                        &helper,
                        RECOVERY_WAIT,
                    )
                    .await
                    .with_context(|| format!("cannot confirm that the earlier work in {} stopped", root.display()))?;
                    tracing::info!(
                        root = %root.display(),
                        waited_ms = started.elapsed().as_millis(),
                        "no helper of an earlier server holds the project"
                    );
                }
                for id in ids {
                    self.end_unrun(&id, State::Interrupted)
                        .await
                        .with_context(|| format!("cannot end request {id}"))?;
                    tracing::info!(request_id = %id, "request was running when the server stopped; it is interrupted");
                }
            }
        }
        for row in rows.into_iter().filter(|row| row.state == State::Queued) {
            let call = match serde_json::from_str::<Map<String, Value>>(&row.arguments) {
                Ok(arguments) => ToolCall::parse(&row.tool, &arguments).ok(),
                Err(_) => None,
            };
            let state = if now_millis() >= row.deadline_at {
                State::TimedOut
            } else if let (true, Some(call)) = (PathBuf::from(&row.root).is_dir(), call) {
                tracing::info!(request_id = %row.id, tool = %row.tool, "starting a queued request");
                self.start(row, call);
                continue;
            } else {
                // The project is gone, or this server cannot read the call.
                State::Interrupted
            };
            self.end_unrun(&row.id, state)
                .await
                .with_context(|| format!("cannot end request {}", row.id))?;
            tracing::info!(request_id = %row.id, state = state.as_str(), "queued request cannot run");
        }
        Ok(())
    }
}
