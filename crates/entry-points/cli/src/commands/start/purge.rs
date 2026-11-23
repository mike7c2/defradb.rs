//! `POST /purge` under `defradb start`: Go's `PurgeAndRestart`.
//!
//! The request records the purge and asks the node to shut down; `run`
//! returns [`RunOutcome::Purged`] only when the Vera node stopped and the
//! database closed; `execute` wipes the store off the data path and starts
//! the node again on the same address. `proofs/tla/StorePurge.tla` models the
//! order, `proofs/lean/StorePurge.lean` the removal set [`wipe_target`] builds.

use std::sync::atomic::{AtomicBool, Ordering};

use nuclei::sync::mpsc;

use super::node::Node;
use crate::config::{Config, DatastoreType};
use crate::error::Result;

/// How a run of the node ended.
#[derive(Debug)]
pub enum RunOutcome {
    /// A signal or a crashed server stopped it.
    Stopped,
    /// A purge stopped it cleanly: wipe this, then start it again.
    Purged(Box<storage::WipeTarget>),
}

/// Records the first purge and asks the node to shut down.
pub(super) struct CliPurge {
    requested: AtomicBool,
    shutdown: mpsc::Sender<()>,
}

impl CliPurge {
    pub(super) fn new(shutdown: mpsc::Sender<()>) -> Self {
        Self {
            requested: AtomicBool::new(false),
            shutdown,
        }
    }

    pub(super) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

impl defra_http::PurgeOperations for CliPurge {
    fn request_purge(&self) -> std::result::Result<(), String> {
        if !self.requested.swap(true, Ordering::SeqCst) {
            // A full channel already holds a shutdown, which this one joins.
            let _ = self.shutdown.try_send(());
        }
        Ok(())
    }
}

/// What a purge wipes: the store on disk (none in memory), the embedded Vera
/// node's chain and journal whatever the store, and never the keyring.
pub(super) fn wipe_target(config: &Config) -> Result<storage::WipeTarget> {
    let store = match config.datastore.store {
        DatastoreType::Memory => None,
        DatastoreType::Regolith => Some((
            config.data_path(),
            Node::encryption_options(config, super::node::regolith_options(config), false)?,
        )),
    };
    let owned_dirs = match config.acp.document_type {
        #[cfg(feature = "embedded-vera")]
        crate::config::AcpDocumentType::VeraRsEmbedded => {
            vec![
                config.rootdir.join("vera"),
                config.rootdir.join("vera-worker"),
            ]
        }
        _ => Vec::new(),
    };
    let target = storage::WipeTarget {
        store,
        owned_dirs,
        kept: vec![config.keyring_path()],
    };
    storage::check_wipe(&target)?;
    Ok(target)
}
