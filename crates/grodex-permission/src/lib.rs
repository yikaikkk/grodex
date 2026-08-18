//! Grodex Permission — approval and policy runtime.
//!
//! Provides the permission check pipeline:
//!   1. Static policy evaluation (Allow / Ask / Deny) with strictest-merge
//!   2. Approval ticket creation with oneshot channels
//!   3. Centralized broker for ticket lifecycle management
//!   4. SQLite-backed ticket persistence for crash recovery
//!   5. ApprovalResolution (Allow/Narrow/Deny/Cancel) → PermissionLease
//!      (single-use, revocation-epoch-bound execution grant)
//!   6. PolicyCompiler → PublishedPolicy with fast indexes (doc 10 §20.13)
//!   7. SessionPolicyGrant for "always allow this session" (doc 10 §20.12)
//!   8. PolicyExplainer for full diagnostic traces (doc 10 §20.14)

pub mod broker;
pub mod compiler;
pub mod manager;
pub mod policy;
pub mod resolution;
pub mod schema;
pub mod session_grant;
pub mod store;
pub mod ticket;

pub use broker::ApprovalBroker;
pub use compiler::{
    CompileResult, PolicyCompiler, PolicyDecisionTrace, PolicyExplainer, PublishedPolicy,
    ReasonCode, SandboxRequirement,
};
pub use manager::{ApprovalRequestedEvent, PermissionManager, PermissionResult, SandboxValidator};
pub use policy::{
    ArgPattern, CommandMatcher, DnsPolicy, HostMatcher, McpArgumentConstraints, McpMatcher,
    MethodClass, NetworkDirection, NetworkMatcher, NetworkProtocol, PermissionPolicy, PolicyRule,
    PortMatcher, RedirectPolicy, ResourceMatcher, SideEffectClass,
};
pub use resolution::{
    ApprovalResolution, LiveRevocationFence, PermissionLease, RevocationAdvanced,
};
pub use schema::{apply_schema, read_schema_version, SCHEMA_VERSION};
pub use session_grant::SessionPolicyGrant;
pub use store::{StoreError, StoreResult, TicketStore};
pub use ticket::{ApprovalTicket, RiskLevel, TicketStatus};
