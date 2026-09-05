/// Controls which `CREATE DATABASE ... STRATEGY` Postgres uses when cloning
/// the template for a fresh test database.
///
/// Postgres 15 introduced two clone strategies:
///
/// - `FILE_COPY`: forces a checkpoint, then copies the template's on-disk
///   files. This is the only strategy that existed before Postgres 15, and
///   it's what `CREATE DATABASE ... TEMPLATE` always did on older servers.
/// - `WAL_LOG`: copies by replaying page changes through WAL instead of a
///   full file copy, and does not force a checkpoint first.
///
/// For template sizes typical of an integration test schema, `WAL_LOG` is
/// both faster on average and dramatically more consistent under load: in
/// our own benchmarking, `FILE_COPY` on a disk-backed (non-tmpfs) instance
/// showed occasional multi-hundred-millisecond stalls that `WAL_LOG` never
/// did, on top of a ~3.5x higher average clone time. See the "Clone
/// strategy" section of the README for the numbers and the benchmark
/// script used to produce them.
///
/// Postgres's own internal heuristic already leans toward `WAL_LOG` for
/// small templates, but the exact size threshold where it flips to
/// `FILE_COPY` is undocumented and not something warmpool wants to depend
/// on implicitly. warmpool pins the strategy explicitly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloneStrategy {
    /// Explicitly request `WAL_LOG`. This is warmpool's default as of
    /// 0.1.1. Requires Postgres 15+; warmpool detects the connected
    /// server's version and silently falls back to
    /// [`CloneStrategy::Auto`] on older servers, since they don't
    /// understand the `STRATEGY` clause at all.
    #[default]
    WalLog,

    /// Explicitly request `FILE_COPY`. Consider this if your template is
    /// large enough that WAL replay would generate more I/O than a
    /// straight file copy — see the README for guidance on where that
    /// crossover tends to sit and how to check it against your own schema.
    FileCopy,

    /// Don't send a `STRATEGY` clause at all; let Postgres's own heuristic
    /// decide. This was warmpool's only behavior prior to 0.1.1, and is
    /// also what any `CloneStrategy` request transparently downgrades to
    /// when the connected server predates Postgres 15.
    Auto,
}

/// The `server_version_num` cutoff below which Postgres does not understand
/// `CREATE DATABASE ... STRATEGY` at all (Postgres 15.0 -> 150000).
const PG15_SERVER_VERSION_NUM: i32 = 150000;

impl CloneStrategy {
    /// Render the SQL fragment to append to `CREATE DATABASE ...`, given
    /// the connected server's numeric version (`server_version_num`, e.g.
    /// `160001` for 16.1). Returns `None` when no clause should be sent:
    /// either the strategy is `Auto`, or the server predates Postgres 15.
    pub(crate) fn sql_clause(self, server_version_num: i32) -> Option<&'static str> {
        if server_version_num < PG15_SERVER_VERSION_NUM {
            return None;
        }
        match self {
            CloneStrategy::WalLog => Some(" STRATEGY = WAL_LOG"),
            CloneStrategy::FileCopy => Some(" STRATEGY = FILE_COPY"),
            CloneStrategy::Auto => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_log_renders_on_pg15_plus() {
        assert_eq!(
            CloneStrategy::WalLog.sql_clause(150000),
            Some(" STRATEGY = WAL_LOG")
        );
        assert_eq!(
            CloneStrategy::WalLog.sql_clause(160003),
            Some(" STRATEGY = WAL_LOG")
        );
    }

    #[test]
    fn wal_log_falls_back_to_auto_before_pg15() {
        assert_eq!(CloneStrategy::WalLog.sql_clause(140009), None);
    }

    #[test]
    fn auto_never_renders_a_clause() {
        assert_eq!(CloneStrategy::Auto.sql_clause(160003), None);
        assert_eq!(CloneStrategy::Auto.sql_clause(140009), None);
    }

    #[test]
    fn file_copy_renders_on_pg15_plus_only() {
        assert_eq!(
            CloneStrategy::FileCopy.sql_clause(150000),
            Some(" STRATEGY = FILE_COPY")
        );
        assert_eq!(CloneStrategy::FileCopy.sql_clause(140009), None);
    }

    #[test]
    fn default_is_wal_log() {
        assert_eq!(CloneStrategy::default(), CloneStrategy::WalLog);
    }
}
