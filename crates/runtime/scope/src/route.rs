use nanocode_durability_core::StoreFuture;

use crate::{Committed, Resident, ScopeError};

pub trait Route<F: Resident>: Send + Sync {
    fn forward<'a>(
        &'a self,
        holder: &'a str,
        scope: &'a str,
        command: F::Command,
    ) -> StoreFuture<'a, Result<Committed<F::Reply>, ScopeError>>;
}
