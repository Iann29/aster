use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPolicy {
    pub version: u64,
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub module_prefixes: Vec<String>,
    #[serde(default)]
    pub insert_tables: Vec<String>,
    #[serde(default = "default_max_reads")]
    pub max_reads_per_transaction: usize,
    #[serde(default = "default_max_writes")]
    pub max_writes_per_transaction: usize,
    #[serde(default = "default_max_scan_limit")]
    pub max_scan_limit: usize,
    #[serde(default = "default_max_concurrent_sessions")]
    pub max_concurrent_sessions: usize,
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
}

const MAX_READS_PER_TRANSACTION: usize = 65_536;
const MAX_WRITES_PER_TRANSACTION: usize = 16_384;
const MAX_SCAN_LIMIT: usize = 10_000;
const MAX_CONCURRENT_SESSIONS: usize = 4_096;

const fn default_max_reads() -> usize {
    4_096
}

const fn default_max_writes() -> usize {
    1_024
}

const fn default_max_scan_limit() -> usize {
    1_000
}

const fn default_max_concurrent_sessions() -> usize {
    1_024
}

const fn default_session_ttl_seconds() -> u64 {
    120
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyError(String);

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

impl DeploymentPolicy {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .map_err(|error| PolicyError(format!("read policy {}: {error}", path.display())))?;
        let policy: Self = serde_json::from_slice(&bytes)
            .map_err(|error| PolicyError(format!("parse policy {}: {error}", path.display())))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.version == 0 {
            return Err(PolicyError(
                "policy version must be greater than zero".into(),
            ));
        }
        validate_prefixes("read_prefixes", &self.read_prefixes)?;
        validate_prefixes("write_prefixes", &self.write_prefixes)?;
        validate_prefixes("module_prefixes", &self.module_prefixes)?;
        validate_table_names(&self.insert_tables)?;
        validate_limit(
            "max_reads_per_transaction",
            self.max_reads_per_transaction,
            MAX_READS_PER_TRANSACTION,
        )?;
        validate_limit(
            "max_writes_per_transaction",
            self.max_writes_per_transaction,
            MAX_WRITES_PER_TRANSACTION,
        )?;
        validate_limit("max_scan_limit", self.max_scan_limit, MAX_SCAN_LIMIT)?;
        validate_limit(
            "max_concurrent_sessions",
            self.max_concurrent_sessions,
            MAX_CONCURRENT_SESSIONS,
        )?;
        if self.session_ttl_seconds == 0 || self.session_ttl_seconds > 3_600 {
            return Err(PolicyError(
                "session_ttl_seconds must be in 1..=3600".into(),
            ));
        }
        Ok(())
    }

    pub fn allows_read(&self, key: &str) -> bool {
        allows_prefix(&self.read_prefixes, key)
    }

    pub fn allows_scan(&self, requested_prefix: &str) -> bool {
        self.read_prefixes
            .iter()
            .any(|allowed| allowed == "*" || requested_prefix.starts_with(allowed))
    }

    pub fn allows_write(&self, key: &str) -> bool {
        allows_prefix(&self.write_prefixes, key)
    }

    pub fn allows_module(&self, path: &str) -> bool {
        allows_prefix(&self.module_prefixes, path)
    }

    pub fn allows_insert(&self, table: &str) -> bool {
        self.insert_tables
            .iter()
            .any(|allowed| allowed == "*" || allowed == table)
    }

    pub fn allow_all_for_tests() -> Self {
        Self {
            version: 1,
            read_prefixes: vec!["*".into()],
            write_prefixes: vec!["*".into()],
            module_prefixes: vec!["*".into()],
            insert_tables: vec!["*".into()],
            max_reads_per_transaction: MAX_READS_PER_TRANSACTION,
            max_writes_per_transaction: MAX_WRITES_PER_TRANSACTION,
            max_scan_limit: MAX_SCAN_LIMIT,
            max_concurrent_sessions: MAX_CONCURRENT_SESSIONS,
            session_ttl_seconds: 3_600,
        }
    }
}

fn validate_limit(name: &str, value: usize, max: usize) -> Result<(), PolicyError> {
    if value == 0 || value > max {
        return Err(PolicyError(format!("{name} must be in 1..={max}")));
    }
    Ok(())
}

fn allows_prefix(prefixes: &[String], value: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| prefix == "*" || value.starts_with(prefix))
}

fn validate_prefixes(name: &str, prefixes: &[String]) -> Result<(), PolicyError> {
    let mut unique = BTreeSet::new();
    for prefix in prefixes {
        if prefix.is_empty() {
            return Err(PolicyError(format!(
                "{name} contains an empty allow-all prefix; production policies must be explicit"
            )));
        }
        if prefix.contains('\0') {
            return Err(PolicyError(format!("{name} contains a NUL byte")));
        }
        if !unique.insert(prefix) {
            return Err(PolicyError(format!(
                "{name} contains duplicate prefix {prefix:?}"
            )));
        }
    }
    Ok(())
}

fn validate_table_names(tables: &[String]) -> Result<(), PolicyError> {
    let mut unique = BTreeSet::new();
    for table in tables {
        if table != "*"
            && (table.is_empty()
                || table.len() > 64
                || !table
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
        {
            return Err(PolicyError(format!(
                "insert_tables contains invalid table name {table:?}"
            )));
        }
        if !unique.insert(table) {
            return Err(PolicyError(format!(
                "insert_tables contains duplicate table {table:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DeploymentPolicy {
        DeploymentPolicy {
            version: 7,
            read_prefixes: vec!["docs/".into()],
            write_prefixes: vec!["docs/public/".into()],
            module_prefixes: vec!["functions/".into()],
            max_reads_per_transaction: 10,
            max_writes_per_transaction: 2,
            max_scan_limit: 100,
            max_concurrent_sessions: 4,
            session_ttl_seconds: 60,
            insert_tables: vec!["docs".into()],
        }
    }

    #[test]
    fn prefix_policy_is_directionally_safe_for_scans() {
        let policy = policy();
        assert!(policy.allows_read("docs/a"));
        assert!(!policy.allows_read("secrets/a"));
        assert!(policy.allows_scan("docs/public/"));
        assert!(!policy.allows_scan("doc"));
        assert!(!policy.allows_scan("secrets/"));
    }

    #[test]
    fn write_and_module_authority_are_independent() {
        let policy = policy();
        assert!(policy.allows_write("docs/public/a"));
        assert!(!policy.allows_write("docs/private/a"));
        assert!(policy.allows_module("functions/messages.js"));
        assert!(!policy.allows_module("admin/messages.js"));
        assert!(policy.allows_insert("docs"));
        assert!(!policy.allows_insert("docs_private"));
    }

    #[test]
    fn explicit_wildcard_grants_deployment_wide_document_authority() {
        let mut candidate = policy();
        candidate.read_prefixes = vec!["*".into()];
        candidate.write_prefixes = vec!["*".into()];
        candidate.module_prefixes = vec!["*".into()];
        candidate.insert_tables = vec!["*".into()];
        candidate.validate().expect("explicit wildcard is valid");
        assert!(candidate.allows_read("canonical-idv6-without-table-prefix"));
        assert!(candidate.allows_scan("any-prefix"));
        assert!(candidate.allows_write("another-id"));
        assert!(candidate.allows_module("any/module.js"));
        assert!(candidate.allows_insert("any_table"));
    }

    #[test]
    fn empty_grant_lists_are_valid_and_deny_everything() {
        let mut candidate = policy();
        candidate.read_prefixes.clear();
        candidate.write_prefixes.clear();
        candidate.module_prefixes.clear();
        candidate.insert_tables.clear();
        candidate.validate().expect("explicit deny-all policy");
        assert!(!candidate.allows_read("docs/a"));
        assert!(!candidate.allows_scan("docs/"));
        assert!(!candidate.allows_write("docs/a"));
        assert!(!candidate.allows_module("functions/messages.js"));
        assert!(!candidate.allows_insert("docs"));
    }

    #[test]
    fn strict_validation_rejects_runaway_resource_limits() {
        let mut candidate = policy();
        candidate.max_reads_per_transaction = MAX_READS_PER_TRANSACTION + 1;
        assert!(candidate.validate().is_err());
        candidate.max_reads_per_transaction = 1;
        candidate.max_concurrent_sessions = 0;
        assert!(candidate.validate().is_err());
    }

    #[test]
    fn strict_validation_rejects_implicit_allow_all_and_duplicates() {
        let mut candidate = policy();
        candidate.read_prefixes = vec![String::new()];
        assert!(candidate.validate().is_err());
        candidate.read_prefixes = vec!["docs/".into(), "docs/".into()];
        assert!(candidate.validate().is_err());
    }
}
