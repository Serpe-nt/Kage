//! The store's error type.

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database refused or could not run the statement.
    #[error("database error: {0}")]
    Database(#[source] sqlx::Error),
    /// A row with the same unique key already exists.
    #[error("conflict on {constraint}")]
    Conflict {
        /// Name of the violated constraint.
        constraint: String,
    },
    /// A migration could not be applied.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A stored value does not parse back into its type. The schema's CHECK
    /// constraints make this unreachable for data written by this version.
    #[error("stored value in {table}.{column} is invalid: {reason}")]
    Corrupt {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
        /// What is wrong with the value.
        reason: String,
    },
    /// A value is too large for its column.
    #[error("{column} is out of range for {table}")]
    OutOfRange {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
    },
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(database) = &error
            && database.is_unique_violation()
        {
            return Self::Conflict {
                constraint: database
                    .constraint()
                    .unwrap_or("unique constraint")
                    .to_owned(),
            };
        }
        Self::Database(error)
    }
}

impl StoreError {
    /// Whether the error is a unique-key conflict.
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::StoreError;

    #[test]
    fn non_database_errors_are_not_conflicts() {
        let error = StoreError::from(sqlx::Error::RowNotFound);

        assert!(matches!(error, StoreError::Database(_)));
        assert!(!error.is_conflict());
        assert!(error.to_string().starts_with("database error"));
    }

    #[test]
    fn messages_name_the_table_and_column() {
        let corrupt = StoreError::Corrupt {
            table: "proxies",
            column: "state",
            reason: "unknown ProxyState \"zombie\"".to_owned(),
        };
        assert_eq!(
            corrupt.to_string(),
            "stored value in proxies.state is invalid: unknown ProxyState \"zombie\""
        );

        let range = StoreError::OutOfRange {
            table: "jobs",
            column: "items_total",
        };
        assert_eq!(range.to_string(), "items_total is out of range for jobs");

        let conflict = StoreError::Conflict {
            constraint: "sessions_pkey".to_owned(),
        };
        assert!(conflict.is_conflict());
        assert_eq!(conflict.to_string(), "conflict on sessions_pkey");
    }
}
