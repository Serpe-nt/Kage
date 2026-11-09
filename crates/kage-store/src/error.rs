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
    /// The store's sessions carry no name of its own, so its sessions
    /// cannot be told from another process's.
    #[error("the store's sessions carry no name of its own")]
    Unnamed,
    /// The database did not answer within the time given.
    #[error("the database did not answer within {0:?}")]
    Unreachable(std::time::Duration),
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

    /// Whether the database could not be reached or could not run the
    /// statement now, whatever the statement: the connection failed or was
    /// ended, the pool had none to give or was closed (a fenced store), or
    /// Postgres refused for the moment (out of connections or resources,
    /// shutting down, a serialization failure or a deadlock). Such a
    /// failure says nothing about what was written, and the same write may
    /// succeed later. Every other error is the statement's own and fails
    /// again the same way (QA round four lock-01, review of #98).
    pub fn is_connection(&self) -> bool {
        match self {
            Self::Unreachable(_) => true,
            Self::Database(error) => match error {
                sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::Protocol(_)
                | sqlx::Error::PoolTimedOut
                | sqlx::Error::PoolClosed
                | sqlx::Error::WorkerCrashed
                | sqlx::Error::BeginFailed => true,
                sqlx::Error::Database(database) => database
                    .code()
                    .is_some_and(|code| transient_sqlstate(&code)),
                _ => false,
            },
            _ => false,
        }
    }
}

/// Whether a SQLSTATE says that the database cannot run the statement now:
/// class 08 (connection exception), 53 (insufficient resources), 57P01 to
/// 57P05 (an administrator, a crash or an idle timeout ended the session),
/// 40001 (serialization failure) and 40P01 (deadlock).
fn transient_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || code.starts_with("53")
        || matches!(
            code,
            "57P01" | "57P02" | "57P03" | "57P04" | "57P05" | "40001" | "40P01"
        )
}

#[cfg(test)]
mod tests {
    use super::StoreError;

    /// Review of #98: only a failure of the connection, never one of the
    /// statement, is a store that cannot be reached.
    #[test]
    fn only_a_failed_connection_is_a_store_that_cannot_be_reached() {
        for reached in [
            StoreError::from(sqlx::Error::PoolClosed),
            StoreError::from(sqlx::Error::PoolTimedOut),
            StoreError::from(sqlx::Error::WorkerCrashed),
            StoreError::from(sqlx::Error::Io(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            ))),
            StoreError::from(sqlx::Error::Protocol("unexpected end".to_owned())),
            StoreError::Unreachable(std::time::Duration::from_secs(3)),
        ] {
            assert!(reached.is_connection(), "{reached}");
        }
        for own in [
            StoreError::from(sqlx::Error::RowNotFound),
            StoreError::Conflict {
                constraint: "pk".to_owned(),
            },
            StoreError::Corrupt {
                table: "jobs",
                column: "state",
                reason: "unknown".to_owned(),
            },
            StoreError::OutOfRange {
                table: "jobs",
                column: "items_total",
            },
            StoreError::Unnamed,
        ] {
            assert!(!own.is_connection(), "{own}");
        }
        for (code, transient) in [
            ("08006", true),
            ("08P01", true),
            ("53300", true),
            ("57P01", true),
            ("57P03", true),
            ("40001", true),
            ("40P01", true),
            ("P0001", false),
            ("23505", false),
            ("22P02", false),
            ("42501", false),
            ("57014", false),
        ] {
            assert_eq!(super::transient_sqlstate(code), transient, "{code}");
        }
    }

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
