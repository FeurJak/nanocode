#[doc(hidden)]
pub(crate) mod compaction;
#[doc(hidden)]
pub(crate) mod context;

mod builder;
mod response;
#[doc(hidden)]
pub(crate) mod state;
pub mod working_state;

pub use builder::{ResponseTurn, Session, SessionBuildError, SessionBuilder};
pub use compaction::{install_history_with_provenance, truncate_middle_with_token_budget};
pub use context::{TOOL_OUTPUT_TOKEN_LIMIT, folded_item, with_serialization_allowance};
pub use response::{
    CompletedCompaction, CompletedResponse, Response, ResponseError, ResponseErrorKind,
    ResponseInput,
};
pub use state::{SessionId, SessionIdError};
