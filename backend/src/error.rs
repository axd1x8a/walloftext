#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadInput(String),
    #[error("quota: {0}")]
    Quota(String),
}
