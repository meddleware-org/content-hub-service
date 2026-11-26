use crate::config::ConfigError;

/// A store failure. No variant carries the database URL or a password.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database URL was refused.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The database answered with an error.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// A migration failed or does not match what was applied.
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// A store result.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;
