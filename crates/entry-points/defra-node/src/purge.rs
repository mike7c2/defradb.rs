//! Purge: wipe the store and restart the node, as Go's `PurgeAndRestart`.
//!
//! A purge is asked for ([`Node::purge_as`] or `POST /purge`) and answered at
//! once: the gate records the first request and tells the embedding
//! application through its [`PurgeHandler`]. The application then calls
//! [`Node::shutdown_and_wipe`] outside the request and builds the node again
//! with the same settings. `proofs/tla/StorePurge.tla` models the order: the
//! wipe runs only after a clean shutdown, and the node opens only after it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use acp::nac::NodePermission;
use defra_core::RequestContext;

use crate::{Error, Node, Result};

/// Told once when a purge is accepted. The application then calls
/// [`Node::shutdown_and_wipe`], outside the request, and builds the node again.
pub trait PurgeHandler: Send + Sync {
    /// A purge was accepted; called once per node.
    fn purge_requested(&self);
}

/// Records the first accepted purge and hands it to the handler once.
pub(crate) struct PurgeGate {
    handler: Option<Arc<dyn PurgeHandler>>,
    requested: AtomicBool,
}

impl PurgeGate {
    pub(crate) fn new(handler: Option<Arc<dyn PurgeHandler>>) -> Arc<Self> {
        Arc::new(Self {
            handler,
            requested: AtomicBool::new(false),
        })
    }

    pub(crate) fn has_handler(&self) -> bool {
        self.handler.is_some()
    }

    /// Accepts a purge. Only the first one reaches the handler; a repeat is
    /// accepted and changes nothing.
    pub(crate) fn request(&self) -> Result<()> {
        let handler = self.handler.as_ref().ok_or(Error::PurgeNeedsHandler)?;
        if !self.requested.swap(true, Ordering::SeqCst) {
            handler.purge_requested();
        }
        Ok(())
    }
}

#[cfg(feature = "http")]
impl defra_http::PurgeOperations for PurgeGate {
    fn request_purge(&self) -> std::result::Result<(), String> {
        self.request().map_err(|error| error.to_string())
    }
}

impl Node {
    /// Asks for a purge as `context`: the node's store is wiped and the
    /// application restarts the node. Needs `DocumentUpdate` under node ACP
    /// and a [`PurgeHandler`] (`NodeBuilder::with_purge_handler`); answered
    /// before anything is wiped.
    pub async fn purge_as(&self, context: &RequestContext) -> Result<()> {
        if !self.purge.has_handler() {
            return Err(Error::PurgeNeedsHandler);
        }
        let (database, context) = (self.database.clone(), self.caller(context));
        self.on_pool(move || async move {
            database
                .check_node_access_as(&context, NodePermission::DocumentUpdate)
                .await
                .map_err(Error::from)
        })
        .await?;
        self.purge.request()
    }

    /// Shuts the node down, then wipes its store: drops every key of a store
    /// on disk and removes the directories the node owns (the embedded Vera
    /// node's). Never wipes after a shutdown that failed, or one another
    /// caller ran. Build the node again with the same settings afterwards.
    ///
    /// The store's directory stays locked until the node is dropped, so
    /// `self` must be the last handle to the node.
    pub async fn shutdown_and_wipe(self: Arc<Self>) -> Result<()> {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            self.wait_for_shutdown().await;
            return Err(Error::WipeAfterShutdown);
        }
        self.teardown().await?;
        let target = self.wipe_target.clone();
        drop(Arc::into_inner(self).ok_or(Error::WipeWhileHeld)?);
        edge::on_demand()?
            .spawn_blocking(move || storage::wipe(&target))
            .await?
            .map_err(Error::Wipe)
    }
}
