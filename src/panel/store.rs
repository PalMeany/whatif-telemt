//! Persisted panel state: operators, linked nodes, and this node's identity.
//!
//! The store is one JSON document rewritten atomically. It holds credential
//! material (password hashes, TOTP secrets, cluster link keys), so it is
//! created with owner-only permissions and every rewrite goes through a
//! temporary file in the same directory followed by a rename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::error::{ProxyError, Result};

use super::rbac::Role;

/// On-disk schema version. Bumped only for incompatible changes.
pub(crate) const STORE_VERSION: u32 = 1;

/// Password hashing algorithm recorded with every credential.
pub(crate) const PASSWORD_ALGORITHM: &str = "pbkdf2-sha256";

/// This node's stable identity inside a federation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NodeIdentity {
    /// Stable identifier a master uses to address this node.
    pub(crate) id: String,
    /// Display name; falls back to the identifier when empty.
    pub(crate) name: String,
    /// Base64url HMAC key that authenticates inbound cluster requests.
    pub(crate) link_key: String,
    /// Unix seconds the identity was minted at.
    pub(crate) created_at: u64,
}

/// One stored password credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PasswordRecord {
    /// Algorithm name; only [`PASSWORD_ALGORITHM`] is accepted.
    pub(crate) algorithm: String,
    /// PBKDF2 work factor this credential was derived with.
    pub(crate) iterations: u32,
    /// Base64url salt.
    pub(crate) salt: String,
    /// Base64url derived key.
    pub(crate) hash: String,
    /// Unix seconds the credential was last set at.
    pub(crate) updated_at: u64,
}

/// One operator's second factor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TotpRecord {
    /// Base32 shared secret.
    pub(crate) secret: String,
    /// False while the operator has scanned but not yet proven the secret.
    pub(crate) confirmed: bool,
    /// SHA-256 hashes of the single-use recovery codes still unspent.
    #[serde(default)]
    pub(crate) recovery_hashes: Vec<String>,
}

/// One panel operator account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OperatorRecord {
    /// Stable identifier, independent of the login name.
    pub(crate) id: String,
    /// Login name.
    pub(crate) username: String,
    /// Role deciding which panel routes and Control API verbs are reachable.
    pub(crate) role: Role,
    /// Password credential.
    pub(crate) password: PasswordRecord,
    /// Forces a password change before anything else becomes reachable.
    #[serde(default)]
    pub(crate) must_change_password: bool,
    /// Second factor, once enrolled.
    #[serde(default)]
    pub(crate) totp: Option<TotpRecord>,
    /// Suspends the account without deleting its audit history.
    #[serde(default)]
    pub(crate) disabled: bool,
    /// Unix seconds the account was created at.
    pub(crate) created_at: u64,
    /// Unix seconds of the last successful login.
    #[serde(default)]
    pub(crate) last_login_at: Option<u64>,
}

impl OperatorRecord {
    /// True when the account has a confirmed second factor.
    pub(crate) fn has_totp(&self) -> bool {
        self.totp.as_ref().is_some_and(|totp| totp.confirmed)
    }
}

/// One node linked into this panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LinkedNode {
    /// The remote node's own identifier, as reported at link time.
    pub(crate) id: String,
    /// Operator-chosen display name.
    pub(crate) name: String,
    /// Base URL of the remote panel, without the `/cluster/v1` suffix.
    pub(crate) url: String,
    /// Base64url HMAC key shared with the remote node.
    pub(crate) link_key: String,
    /// Lowercase hex SHA-256 of the remote leaf certificate, when pinned.
    #[serde(default)]
    pub(crate) fingerprint: Option<String>,
    /// Free-form operator labels used for grouping in the UI.
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    /// Unix seconds the link was created at.
    pub(crate) added_at: u64,
}

/// Runtime-editable preferences that do not warrant a config reload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PanelSettings {
    /// Node selected when the UI opens.
    #[serde(default)]
    pub(crate) default_node_id: Option<String>,
    /// Per-operator interface preferences, keyed by operator id.
    #[serde(default)]
    pub(crate) appearance: BTreeMap<String, String>,
}

/// The whole persisted document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PanelStoreData {
    /// Schema version.
    pub(crate) version: u32,
    /// This node's identity.
    pub(crate) node: NodeIdentity,
    /// Operator accounts.
    #[serde(default)]
    pub(crate) operators: Vec<OperatorRecord>,
    /// Nodes linked into this panel.
    #[serde(default)]
    pub(crate) nodes: Vec<LinkedNode>,
    /// Runtime-editable preferences.
    #[serde(default)]
    pub(crate) settings: PanelSettings,
}

impl PanelStoreData {
    /// Finds an operator by login name.
    pub(crate) fn operator_by_username(&self, username: &str) -> Option<&OperatorRecord> {
        self.operators
            .iter()
            .find(|operator| operator.username == username)
    }

    /// Finds an operator by identifier.
    pub(crate) fn operator_by_id(&self, id: &str) -> Option<&OperatorRecord> {
        self.operators.iter().find(|operator| operator.id == id)
    }

    /// Finds a mutable operator by identifier.
    pub(crate) fn operator_by_id_mut(&mut self, id: &str) -> Option<&mut OperatorRecord> {
        self.operators.iter_mut().find(|operator| operator.id == id)
    }

    /// Finds a linked node by identifier.
    pub(crate) fn node_by_id(&self, id: &str) -> Option<&LinkedNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// True when at least one enabled administrator remains.
    ///
    /// Checked before every demotion and deletion: an operator who removes the
    /// last administrator locks the panel out of its own account management.
    pub(crate) fn has_other_active_admin(&self, excluding_id: &str) -> bool {
        self.operators.iter().any(|operator| {
            operator.id != excluding_id && operator.role == Role::Admin && !operator.disabled
        })
    }
}

/// Reads the store from disk, returning `None` when the file does not exist.
pub(crate) async fn load(path: &Path) -> Result<Option<PanelStoreData>> {
    let content = match tokio::fs::read(path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ProxyError::Config(format!(
                "failed to read panel store {}: {error}",
                path.display()
            )));
        }
    };
    let data: PanelStoreData = serde_json::from_slice(&content).map_err(|error| {
        ProxyError::Config(format!(
            "panel store {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    if data.version != STORE_VERSION {
        return Err(ProxyError::Config(format!(
            "panel store {} has version {}, expected {}",
            path.display(),
            data.version,
            STORE_VERSION
        )));
    }
    Ok(Some(data))
}

/// Rewrites the store atomically with owner-only permissions.
pub(crate) async fn save(path: &Path, data: &PanelStoreData) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(data)
        .map_err(|error| ProxyError::Internal(format!("failed to encode panel store: {error}")))?;
    write_private_atomic(path, &serialized).await
}

/// Writes `content` to `path` through a same-directory temporary file.
///
/// The rename is what makes the swap atomic; writing in place would leave a
/// truncated store behind if the process died mid-write, and a truncated store
/// is an unopenable panel.
pub(crate) async fn write_private_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = temporary_path(path);
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| {
            ProxyError::Config(format!(
                "failed to create panel directory {}: {error}",
                directory.display()
            ))
        })?;
    restrict_directory(directory).await;
    tokio::fs::write(&temporary, content)
        .await
        .map_err(|error| {
            ProxyError::Config(format!("failed to write {}: {error}", temporary.display()))
        })?;
    restrict_file(&temporary).await;
    tokio::fs::rename(&temporary, path).await.map_err(|error| {
        ProxyError::Config(format!("failed to replace {}: {error}", path.display()))
    })?;
    Ok(())
}

/// Builds the temporary sibling path used by [`write_private_atomic`].
fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "panel".to_string());
    name.push_str(".tmp");
    path.with_file_name(name)
}

/// Applies owner-only permissions to a file holding credential material.
pub(crate) async fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
        {
            warn!(path = %path.display(), %error, "Failed to restrict panel file permissions");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Applies owner-only permissions to the panel data directory.
async fn restrict_directory(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await
        {
            warn!(path = %path.display(), %error, "Failed to restrict panel directory permissions");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Warns when the store is readable by anyone but its owner.
pub(crate) async fn audit_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = tokio::fs::metadata(path).await else {
            return;
        };
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            warn!(
                path = %path.display(),
                mode = format!("{mode:04o}"),
                "Panel store is readable beyond its owner; it holds password hashes and link keys"
            );
        } else {
            info!(path = %path.display(), "Panel store loaded");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator(id: &str, role: Role, disabled: bool) -> OperatorRecord {
        OperatorRecord {
            id: id.to_string(),
            username: id.to_string(),
            role,
            password: PasswordRecord {
                algorithm: PASSWORD_ALGORITHM.to_string(),
                iterations: 100_000,
                salt: "c2FsdA".to_string(),
                hash: "aGFzaA".to_string(),
                updated_at: 0,
            },
            must_change_password: false,
            totp: None,
            disabled,
            created_at: 0,
            last_login_at: None,
        }
    }

    fn store(operators: Vec<OperatorRecord>) -> PanelStoreData {
        PanelStoreData {
            version: STORE_VERSION,
            node: NodeIdentity {
                id: "node".to_string(),
                name: "node".to_string(),
                link_key: "a2V5".to_string(),
                created_at: 0,
            },
            operators,
            nodes: Vec::new(),
            settings: PanelSettings::default(),
        }
    }

    #[test]
    fn last_active_admin_is_recognised() {
        let data = store(vec![
            operator("root", Role::Admin, false),
            operator("second", Role::Admin, true),
            operator("viewer", Role::Viewer, false),
        ]);
        assert!(!data.has_other_active_admin("root"));
        assert!(data.has_other_active_admin("viewer"));
    }

    #[tokio::test]
    async fn store_round_trips_through_an_atomic_rewrite() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("panel.json");
        assert!(load(&path).await.expect("load").is_none());
        let data = store(vec![operator("root", Role::Admin, false)]);
        save(&path, &data).await.expect("save");
        let loaded = load(&path).await.expect("load").expect("present");
        assert_eq!(loaded.operators.len(), 1);
        assert_eq!(loaded.node.id, "node");
        assert!(!directory.path().join("panel.json.tmp").exists());
    }

    #[tokio::test]
    async fn a_foreign_schema_version_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("panel.json");
        let mut data = store(Vec::new());
        data.version = STORE_VERSION + 1;
        save(&path, &data).await.expect("save");
        assert!(load(&path).await.is_err());
    }
}
