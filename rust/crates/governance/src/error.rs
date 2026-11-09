/// Failure of a governance port. The text is the stable, user-visible
/// message; hosts convert it to their own error type verbatim.
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    #[error("{0}")]
    Execution(String),
}

pub type GovernanceResult<T> = Result<T, GovernanceError>;
