//! Recovery at start, before the server listens: end the rows an earlier
//! server left, then start the queued ones.
//!
//! A row that says `running` belonged to an earlier server. Before recovery
//! runs, the server took its store ([`crate::store::hold`]), which waits until
//! every process the earlier server started has exited, so none of that work
//! still runs. The row becomes `interrupted`; it never runs again. A `queued`
//! row never started, so recovery starts it in `created_at` order, unless its
//! deadline has passed (`timed_out`) or its project is gone (`interrupted`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use serde_json::{Map, Value};

use super::{RequestRunner, State, now_millis};
use crate::server::ToolCall;

impl RequestRunner {
    /// Prune the table, end the rows an earlier server left running, and start
    /// the queued ones.
    ///
    /// # Errors
    /// Fails when the table cannot be read or written.
    pub async fn recover(self: &Arc<Self>) -> anyhow::Result<()> {
        self.prune(true).await;
        let rows = self.store.unfinished().await.context("cannot read the request table")?;
        for row in rows.iter().filter(|row| row.state == State::Running) {
            self.end_unrun(&row.id, State::Interrupted)
                .await
                .with_context(|| format!("cannot end request {}", row.id))?;
            tracing::info!(request_id = %row.id, "request was running when the server stopped; it is interrupted");
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
