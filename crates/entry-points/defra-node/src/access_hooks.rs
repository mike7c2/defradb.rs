use std::sync::Arc;

use db::merge::governance::{MergeGovernance, MergeValidator};
use query::access_hooks::{ReadValidator, WriteValidator};

/// An app's own access control for an iroh node.
///
/// The collections named in [`AccessHooks::new`], and those a rule given to
/// [`AccessHooks::claiming`] accepts, are governed by the app:
/// replicated composites into them are judged by the merge validator, never
/// by ACP, and a governed collection with no merge validator merges nothing.
/// The read and write validators are node-local and narrow what this node's
/// clients may do; they compose with ACP and never widen it.
#[derive(Clone, Default)]
pub struct AccessHooks {
    governed: Vec<String>,
    rule: Option<db::merge::governance::ClaimRule>,
    merge: Option<Arc<dyn MergeValidator>>,
    read: Option<Arc<dyn ReadValidator>>,
    write: Option<Arc<dyn WriteValidator>>,
}

impl AccessHooks {
    /// Claim collections, by name, for the app's merge validator.
    pub fn new(governed: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            governed: governed.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Claim, for the app's merge validator, every collection the rule
    /// accepts, including one defined after the node started.
    ///
    /// The rule must depend on the collection's own definition alone: see
    /// [`MergeGovernance::claiming`].
    pub fn claiming(
        rule: impl Fn(&schema::CollectionVersion) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self::default().with_claim_rule(rule)
    }

    /// Also claim every collection the rule accepts, beside those named.
    pub fn with_claim_rule(
        mut self,
        rule: impl Fn(&schema::CollectionVersion) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.rule = Some(Arc::new(rule));
        self
    }

    pub fn with_merge_validator(mut self, validator: Arc<dyn MergeValidator>) -> Self {
        self.merge = Some(validator);
        self
    }

    pub fn with_read_validator(mut self, validator: Arc<dyn ReadValidator>) -> Self {
        self.read = Some(validator);
        self
    }

    pub fn with_write_validator(mut self, validator: Arc<dyn WriteValidator>) -> Self {
        self.write = Some(validator);
        self
    }

    pub(crate) fn install<S>(&self, database: &db::DB<S>)
    where
        S: storage::corekv::Store,
    {
        let mut governance = MergeGovernance::new(self.governed.iter().cloned());
        if let Some(rule) = &self.rule {
            let rule = rule.clone();
            governance = governance.with_rule(move |collection| rule(collection));
        }
        database.set_merge_governance(match &self.merge {
            Some(validator) => governance.with_validator(validator.clone()),
            None => governance,
        });
    }

    pub(crate) fn apply<F, R>(&self, runner: query::QueryRunner<F, R>) -> query::QueryRunner<F, R>
    where
        F: query::runner::DocFetcher + 'static,
        R: query::txn::TransactionRegistry,
    {
        let runner = match &self.read {
            Some(validator) => runner.with_read_validator(validator.clone()),
            None => runner,
        };
        match &self.write {
            Some(validator) => runner.with_write_validator(validator.clone()),
            None => runner,
        }
    }
}
