/** Shapes shared between the panel routes and the telemt Control API. */

export type Role = "viewer" | "operator" | "admin";

export type SessionView = {
  operator_id: string;
  username: string;
  role: Role;
  csrf_token: string;
  must_change_password: boolean;
  totp_enabled: boolean;
  totp_required: boolean;
  created_at: number;
  expires_at: number;
};

export type BootstrapView = {
  version: string;
  started_at: number;
  bundled_ui: boolean;
  node: {
    id: string;
    name: string;
    cluster_enabled: boolean;
    role: "standalone" | "master" | "agent" | "master-agent";
    is_master: boolean;
    is_agent: boolean;
    linked_nodes: number;
  };
  operator: SessionView;
  default_node_id: string | null;
  audit_enabled: boolean;
};

export type NodeView = {
  id: string;
  name: string;
  kind: "local" | "linked";
  url: string | null;
  tags: string[];
  pinned: boolean;
  added_at: number;
  reachable: boolean;
  checked_at: number;
  latency_ms: number | null;
  version: string | null;
  error: string | null;
};

export type OperatorView = {
  id: string;
  username: string;
  role: Role;
  disabled: boolean;
  must_change_password: boolean;
  totp_enabled: boolean;
  created_at: number;
  last_login_at: number | null;
  active_sessions: number;
};

export type AuditRecord = {
  seq: number;
  ts: number;
  actor: string;
  actor_id: string;
  action: string;
  target: string;
  node: string;
  result: string;
  address: string;
  detail: string;
  prev: string;
  hash: string;
};

export type UserLinks = {
  tg_classic?: string | null;
  tg_secure?: string | null;
  tg_tls?: string | null;
  tls_domains?: { domain: string; link: string }[];
  [key: string]: unknown;
};

export type UserInfo = {
  username: string;
  enabled: boolean;
  in_runtime: boolean;
  user_ad_tag: string | null;
  max_tcp_conns: number | null;
  expiration_rfc3339: string | null;
  data_quota_bytes: number | null;
  rate_limit_up_bps: number | null;
  rate_limit_down_bps: number | null;
  max_unique_ips: number | null;
  current_connections: number;
  active_unique_ips: number;
  active_unique_ips_list: string[];
  recent_unique_ips: number;
  recent_unique_ips_list: string[];
  total_octets: number;
  links: UserLinks;
};

export type SummaryData = {
  uptime_seconds: number;
  connections_total: number;
  connections_bad_total: number;
  connections_bad_by_class: { class: string; total: number }[];
  handshake_failures_by_class: { class: string; total: number }[];
  handshake_timeouts_total: number;
  configured_users: number;
};

export type HealthReadyData = {
  ready: boolean;
  status: string;
  reason: string | null;
  admission_open: boolean;
  healthy_upstreams: number;
  total_upstreams: number;
};

export type OverviewRow = {
  node_id: string;
  node_name: string;
  reachable: boolean;
  error: string | null;
  summary: SummaryData | null;
  ready: HealthReadyData | null;
};

export type ReloadAccepted = {
  reload_id: number;
  [key: string]: unknown;
};

export type RuntimeEvent = {
  epoch_secs?: number;
  ts?: number;
  kind?: string;
  event?: string;
  detail?: string;
  message?: string;
  [key: string]: unknown;
};
