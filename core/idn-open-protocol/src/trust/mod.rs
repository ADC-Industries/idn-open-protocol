//! Site trust (`secure/2`, spec/extensions/idn-extensions.md §E.10): per-device
//! certificates from a site authority in the Discovery Server, with approve, deny and
//! revoke.

mod authority;
#[doc(hidden)]
pub mod options;
pub mod pki;
#[doc(hidden)]
pub mod proto;
pub mod token;
pub use authority::{
    ALERTS, AlertEntry, Authority, AuthorityConfig, AuthorityEvent, EnrolledDevice, JoinRequest,
    KeyGrant, KeyRequest, KeyStatus, NodeReport,
};
pub use proto::{same_code, show_code};
mod join_code;
pub use join_code::{
    IssuedCode, JOIN_MODE_KEY, JoinCodeInfo, JoinMode, normalize as normalize_join_code,
};
mod device;
pub mod pair;
pub use device::{
    DeviceRole, DirEntry, Enrollment, EnrollmentConfig, NodeMachine, PairLease, TrustState,
    install_issued,
};
pub use options::{OptionScope, Resolved, SiteOptions};
