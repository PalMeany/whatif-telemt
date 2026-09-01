//! Process-scoped panel state.
//!
//! Everything the panel owns for the lifetime of the process lives here: the
//! persisted store, the session registry, the login throttle, the audit log,
//! and the resolved endpoint of this node's own Control API. Runtime data of
//! the proxy itself is never cached here — the panel reads it back through the
//! Control API on every request so a configuration reload cannot leave a stale
//! view behind.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::RwLock;
use tracing::info;

use crate::config::{ClusterRole, PanelConfig, ProxyConfig};
use crate::crypto::SecureRandom;
use crate::error::{ProxyError, Result};

use super::audit::{AuditEntry, AuditLog};
use super::cluster::sign::NonceWindow;
use super::crypto::{encode, random_password, random_secret};
use super::password;
use super::ratelimit::{LoginThrottle, ThrottleLimits};
use super::rbac::Role;
use super::session::{SessionLimits, SessionRegistry};
use super::store::{
    self, NodeIdentity, OperatorRecord, PanelSettings, PanelStoreData, STORE_VERSION,
};

/// File name of the persisted panel store.
const STORE_FILE: &str = "panel.json";

/// File name of the audit log.
const AUDIT_FILE: &str = "panel-audit.jsonl";

/// File name the bootstrap credential is written to.
const BOOTSTRAP_FILE: &str = "panel-bootstrap.txt";

/// Login name of the account created on first start.
const BOOTSTRAP_USERNAME: &str = "admin";

/// Length of the generated bootstrap password.
const BOOTSTRAP_PASSWORD_LEN: usize = 24;

/// Where this node's own Control API lives.
#[derive(Debug, Clone)]
pub(crate) struct ControlEndpoint {
    /// Base URL, without a trailing slash.
    pub(crate) url: String,
    /// Value sent in `Authorization`; empty when the API needs none.
    pub(crate) auth_header: String,
}

/// Last observed state of a linked node.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeHealth {
    /// True when the last probe succeeded.
    pub(crate) reachable: bool,
    /// Unix seconds of the last probe.
    pub(crate) checked_at: u64,
    /// Round-trip time of the last successful probe.
    pub(crate) latency_ms: Option<u64>,
    /// Failure detail of the last unsuccessful probe.
    pub(crate) error: Option<String>,
    /// Version string the node reported.
    pub(crate) version: Option<String>,
}

/// Everything the panel owns for the lifetime of the process.
pub(crate) struct PanelState {
    /// Panel section captured at start-up.
    pub(crate) config: PanelConfig,
    /// Address the panel listens on.
    pub(crate) listen: SocketAddr,
    /// This node's own Control API endpoint.
    pub(crate) control: ControlEndpoint,
    /// Path of the persisted store.
    pub(crate) store_path: PathBuf,
    /// Persisted store, guarded for the whole process.
    pub(crate) store: RwLock<PanelStoreData>,
    /// Live operator sessions.
    pub(crate) sessions: SessionRegistry,
    /// Login throttle.
    pub(crate) throttle: LoginThrottle,
    /// Hash-chained audit log.
    pub(crate) audit: AuditLog,
    /// Replay window for inbound cluster requests.
    pub(crate) nonce_window: NonceWindow,
    /// Last observed state of every linked node.
    pub(crate) node_health: Mutex<HashMap<String, NodeHealth>>,
    /// Randomness source shared by every panel subsystem.
    pub(crate) random: Arc<SecureRandom>,
    /// Unix seconds the panel started at.
    pub(crate) started_at: u64,
}

impl PanelState {
    /// Builds the panel state, creating the store and bootstrap account.
    pub(crate) async fn bootstrap(
        config: &ProxyConfig,
        config_dir: Option<&std::path::Path>,
    ) -> Result<Arc<Self>> {
        let panel = config.panel.clone();
        let listen = panel.listen.parse::<SocketAddr>().map_err(|_| {
            ProxyError::Config("panel.listen must be a numeric ip:port".to_string())
        })?;
        let data_dir = resolve_data_dir(&panel, config, config_dir);
        tokio::fs::create_dir_all(&data_dir)
            .await
            .map_err(|error| {
                ProxyError::Config(format!(
                    "failed to create panel data directory {}: {error}",
                    data_dir.display()
                ))
            })?;
        let store_path = data_dir.join(STORE_FILE);
        let random = Arc::new(SecureRandom::new());
        let now = unix_now();

        let mut data = match store::load(&store_path).await? {
            Some(data) => {
                store::audit_permissions(&store_path).await;
                data
            }
            None => PanelStoreData {
                version: STORE_VERSION,
                node: mint_identity(&panel, &random, now),
                operators: Vec::new(),
                nodes: Vec::new(),
                settings: PanelSettings::default(),
            },
        };
        if data.node.link_key.is_empty() {
            data.node.link_key = encode(&random_secret(&random));
        }
        if !panel.cluster.node_name.is_empty() {
            data.node.name = panel.cluster.node_name.clone();
        }

        if data.operators.is_empty() {
            let password = random_password(&random, BOOTSTRAP_PASSWORD_LEN);
            let record =
                password::hash(&password, panel.password_hash_iterations, &random, now).await?;
            // The credential file is written before the store is: if it cannot
            // be written, start-up fails with the store untouched, so the next
            // attempt mints a fresh account rather than leaving one behind whose
            // password nobody knows. The password is never logged.
            write_bootstrap_credential(&data_dir, &password).await?;
            data.operators.push(OperatorRecord {
                id: format!("op-{}", &encode(&random_secret(&random))[..16]),
                username: BOOTSTRAP_USERNAME.to_string(),
                role: Role::Admin,
                password: record,
                must_change_password: true,
                totp: None,
                disabled: false,
                created_at: now,
                last_login_at: None,
            });
        }

        store::save(&store_path, &data).await?;

        let control = resolve_control_endpoint(&panel, config)?;
        let audit = AuditLog::open(
            data_dir.join(AUDIT_FILE),
            panel.audit_max_bytes,
            panel.audit_retention_days,
        )
        .await;

        let state = Arc::new(Self {
            sessions: SessionRegistry::new(
                SessionLimits {
                    ttl_secs: panel.session_ttl_secs,
                    idle_timeout_secs: panel.session_idle_timeout_secs,
                    max_per_operator: panel.max_sessions_per_operator,
                    max_total: panel.max_sessions_total,
                },
                random.clone(),
            ),
            throttle: LoginThrottle::new(ThrottleLimits {
                max_attempts: panel.login_max_attempts,
                lockout_secs: panel.login_lockout_secs,
            }),
            nonce_window: NonceWindow::new(panel.cluster.nonce_capacity),
            node_health: Mutex::new(HashMap::new()),
            audit,
            store: RwLock::new(data),
            store_path,
            control,
            listen,
            config: panel,
            random,
            started_at: now,
        });
        Ok(state)
    }

    /// Persists the current store contents.
    pub(crate) async fn persist(&self) -> Result<()> {
        let data = self.store.read().await;
        store::save(&self.store_path, &data).await
    }

    /// Appends one audit record when auditing is enabled.
    pub(crate) async fn record(&self, entry: AuditEntry) {
        if !self.config.audit_enabled {
            return;
        }
        self.audit.append(entry, unix_now()).await;
    }

    /// Federation role this node actually runs with.
    ///
    /// `panel.cluster.role` is only meaningful while `panel.cluster.enabled` is
    /// set; collapsing the two here keeps every guard from having to remember
    /// that pairing.
    pub(crate) fn cluster_role(&self) -> ClusterRole {
        if self.config.cluster.enabled {
            self.config.cluster.role
        } else {
            ClusterRole::Standalone
        }
    }

    /// True when the request address is allowed to reach the panel at all.
    pub(crate) fn address_allowed(&self, address: IpAddr) -> bool {
        self.config.whitelist.is_empty()
            || self
                .config
                .whitelist
                .iter()
                .any(|network| network.contains(address))
    }

    /// True when the request address may reach the inbound cluster endpoint.
    pub(crate) fn cluster_address_allowed(&self, address: IpAddr) -> bool {
        self.config.cluster.allow_from.is_empty()
            || self
                .config
                .cluster
                .allow_from
                .iter()
                .any(|network| network.contains(address))
    }

    /// Records the outcome of a linked-node probe.
    pub(crate) fn record_node_health(&self, node_id: &str, health: NodeHealth) {
        self.node_health.lock().insert(node_id.to_string(), health);
    }

    /// Reads the last observed state of a linked node.
    pub(crate) fn node_health_of(&self, node_id: &str) -> Option<NodeHealth> {
        self.node_health.lock().get(node_id).cloned()
    }
}

/// Unix seconds, saturating at the epoch on a clock before it.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Unix milliseconds, saturating at the epoch on a clock before it.
pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Resolves the directory holding the store, audit log, and bootstrap file.
fn resolve_data_dir(
    panel: &PanelConfig,
    config: &ProxyConfig,
    config_dir: Option<&std::path::Path>,
) -> PathBuf {
    if !panel.data_dir.is_empty() {
        return PathBuf::from(&panel.data_dir);
    }
    if let Some(data_path) = config.general.data_path.as_ref() {
        return data_path.join("panel");
    }
    config_dir
        .map(|directory| directory.join("panel"))
        .unwrap_or_else(|| PathBuf::from("panel"))
}

/// Mints this node's identity on first start.
fn mint_identity(panel: &PanelConfig, random: &SecureRandom, now: u64) -> NodeIdentity {
    let id = format!("node-{}", &encode(&random_secret(random))[..20]);
    let name = if panel.cluster.node_name.is_empty() {
        hostname().unwrap_or_else(|| id.clone())
    } else {
        panel.cluster.node_name.clone()
    };
    NodeIdentity {
        id,
        name,
        link_key: encode(&random_secret(random)),
        created_at: now,
    }
}

/// Reads the system hostname for the node's default display name.
///
/// Deliberately shallow: this only names a row in the operator's node list, so
/// the environment and `/etc/hostname` are enough and neither adds a dependency
/// nor an `unsafe` call for a label.
fn hostname() -> Option<String> {
    if let Ok(name) = std::env::var("HOSTNAME")
        && !name.trim().is_empty()
    {
        return Some(name.trim().to_string());
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Resolves the Control API endpoint the panel drives this node through.
fn resolve_control_endpoint(panel: &PanelConfig, config: &ProxyConfig) -> Result<ControlEndpoint> {
    let url = if panel.control_api_url.is_empty() {
        let listen = config
            .server
            .api
            .listen
            .parse::<SocketAddr>()
            .map_err(|_| {
                ProxyError::Config("server.api.listen must be a numeric ip:port".into())
            })?;
        // An unspecified bind address is reachable on loopback, and loopback is
        // the only address the panel may assume is its own.
        let host = match listen.ip() {
            IpAddr::V4(address) if address.is_unspecified() => "127.0.0.1".to_string(),
            IpAddr::V6(address) if address.is_unspecified() => "[::1]".to_string(),
            IpAddr::V4(address) => address.to_string(),
            IpAddr::V6(address) => format!("[{address}]"),
        };
        format!("http://{host}:{}", listen.port())
    } else {
        panel.control_api_url.trim_end_matches('/').to_string()
    };
    let auth_header = if panel.control_api_token.is_empty() {
        config.server.api.auth_header.clone()
    } else {
        panel.control_api_token.clone()
    };
    Ok(ControlEndpoint { url, auth_header })
}

/// Writes the first-start credential where only the service account can read it.
async fn write_bootstrap_credential(data_dir: &std::path::Path, password: &str) -> Result<()> {
    let path = data_dir.join(BOOTSTRAP_FILE);
    let content = format!(
        "telemt panel bootstrap credential\nusername: {BOOTSTRAP_USERNAME}\npassword: {password}\n\
         \nThis password must be changed at first login. Delete this file afterwards.\n"
    );
    store::write_private_atomic(&path, content.as_bytes()).await?;
    info!(
        path = %path.display(),
        username = BOOTSTRAP_USERNAME,
        "Panel bootstrap account created; the generated password is in this file"
    );
    Ok(())
}
