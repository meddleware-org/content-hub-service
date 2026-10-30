use crate::config::ConfigError;

/// A store failure. No variant carries the database URL or a password.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;
