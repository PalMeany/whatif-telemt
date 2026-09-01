//! Roles and the permission gate applied to every panel action.
//!
//! The panel forwards to the Control API, so the gate is expressed over the
//! Control API's own method-and-path surface rather than over a duplicate list
//! of panel verbs. A route the Control API grows without a rule here is denied
//! to everyone but an administrator, which is the safe direction to fail in.

use serde::{Deserialize, Serialize};

/// Role assigned to a panel operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    /// Reads every view; changes nothing.
    Viewer,
    /// Reads every view and manages proxy users and their quotas.
    Operator,
    /// Everything, including configuration, node links, and panel accounts.
    Admin,
}

impl Role {
    /// Wire name used in the panel API and in the UI.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    /// Parses a wire name, rejecting anything else.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    /// True when the role carries the permission.
    pub(crate) fn allows(self, permission: Permission) -> bool {
        match self {
            Role::Admin => true,
            Role::Operator => matches!(permission, Permission::ViewNode | Permission::ManageUsers),
            Role::Viewer => matches!(permission, Permission::ViewNode),
        }
    }
}

/// One capability the panel gates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    /// Reads node state through the Control API.
    ViewNode,
    /// Creates, edits, and removes proxy users; resets their quotas.
    ManageUsers,
    /// Edits node configuration and triggers reloads.
    ManageConfig,
    /// Links, edits, and unlinks federated nodes.
    ManageNodes,
    /// Manages panel operator accounts and reads the audit log.
    ManageOperators,
}

impl Permission {
    /// Machine-readable name used in audit records and error payloads.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Permission::ViewNode => "view_node",
            Permission::ManageUsers => "manage_users",
            Permission::ManageConfig => "manage_config",
            Permission::ManageNodes => "manage_nodes",
            Permission::ManageOperators => "manage_operators",
        }
    }
}

/// Resolves the permission a Control API request requires.
///
/// `path` is the Control API path with its `/v1` prefix intact and its query
/// already stripped.
pub(crate) fn control_api_permission(method: &str, path: &str) -> Permission {
    if method == "GET" || method == "HEAD" {
        return Permission::ViewNode;
    }
    if path == "/v1/users" || path.starts_with("/v1/users/") {
        return Permission::ManageUsers;
    }
    // `/v1/config` and every `/v1/system/*` verb reconfigure the node or move
    // it between runtime generations. Nothing below administrator touches them.
    Permission::ManageConfig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_open_to_every_role() {
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert!(role.allows(control_api_permission("GET", "/v1/stats/summary")));
        }
    }

    #[test]
    fn user_management_stops_at_viewer() {
        let permission = control_api_permission("POST", "/v1/users");
        assert_eq!(permission, Permission::ManageUsers);
        assert!(!Role::Viewer.allows(permission));
        assert!(Role::Operator.allows(permission));
        assert!(Role::Admin.allows(permission));
    }

    #[test]
    fn configuration_and_reloads_are_administrator_only() {
        for (method, path) in [
            ("PATCH", "/v1/config"),
            ("POST", "/v1/system/reload"),
            ("DELETE", "/v1/system/reload/7"),
        ] {
            let permission = control_api_permission(method, path);
            assert_eq!(permission, Permission::ManageConfig);
            assert!(!Role::Operator.allows(permission));
            assert!(Role::Admin.allows(permission));
        }
    }

    #[test]
    fn an_unknown_mutating_route_falls_back_to_the_strictest_permission() {
        let permission = control_api_permission("POST", "/v1/something/new");
        assert_eq!(permission, Permission::ManageConfig);
        assert!(!Role::Operator.allows(permission));
    }

    #[test]
    fn role_names_round_trip() {
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("root"), None);
    }
}
