pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying bridge error is intentionally discarded so a malformed
    /// native response can never carry token material into logs.
    #[error("native Google authorization failed")]
    NativeBridge,
}
