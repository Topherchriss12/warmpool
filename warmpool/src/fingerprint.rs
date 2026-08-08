use sha2::{Digest, Sha256};
use sqlx::migrate::Migration;

/// Hash the ordered (version, description, checksum) triples of every
/// migration into a short hex fingerprint. Two invocations with identical
/// migrations always produce the same fingerprint, and any change to a
/// migration's SQL (which changes its checksum) produces a different one. This
/// fingerprint is used to derive a deterministic template database name, which
/// is what makes the template cache self invalidating: stale templates just
/// never get a name collision with the new migration set, so they sit unused in
/// Postgres until you clean them up (see the README's note on stale template cleanup).
pub fn fingerprint(migrations: &[Migration], salt: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    if let Some(salt) = salt {
        hasher.update(salt.as_bytes());
        hasher.update(b"\0");
    }
    for migration in migrations {
        hasher.update(migration.version.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(migration.description.as_bytes());
        hasher.update(b"\0");
        hasher.update(migration.checksum.as_ref());
        hasher.update(b"\0");
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Derive a Postgres advisory lock key deterministically from the
/// fingerprint. This means two unrelated
/// projects that both depend on warmpool and happen to point at the same
/// Postgres instance (a shared CI service container, say) don't contend on
/// the same lock key just because they both used the default, they only
/// collide if they have the exact same migration set, in which case
/// contending on the build is a good safeguard against both trying to build the same template at once.
pub fn lock_key_from_fingerprint(fingerprint: &str) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"warmpool-advisory-lock\0");
    hasher.update(fingerprint.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[0..8]);
    i64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        let migrations: Vec<Migration> = Vec::new();
        assert_eq!(fingerprint(&migrations, None), fingerprint(&migrations, None));
    }

    #[test]
    fn salt_changes_fingerprint() {
        let migrations: Vec<Migration> = Vec::new();
        assert_ne!(
            fingerprint(&migrations, Some("a")),
            fingerprint(&migrations, Some("b"))
        );
    }

    #[test]
    fn lock_key_is_deterministic_per_fingerprint() {
        assert_eq!(
            lock_key_from_fingerprint("abc123"),
            lock_key_from_fingerprint("abc123")
        );
        assert_ne!(
            lock_key_from_fingerprint("abc123"),
            lock_key_from_fingerprint("def456")
        );
    }
}
