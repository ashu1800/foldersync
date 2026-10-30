//! fs-sync-daemon：编排层（core + net + store 组合，供桌面与 CLI 共用）。

pub mod engine;
pub mod pairing;
pub mod relay_sync;
pub mod runtime;
pub mod session;
pub mod ssh_server;
pub mod sync;
pub mod trust;
pub mod watcher;

pub use engine::{SyncEngine, WriteSuppression, ensure_within};
pub use pairing::{PairingManager, PendingPairing, fingerprint_str};
pub use relay_sync::relay_sync_round;
pub use runtime::{FolderRuntime, PeerLoopConfig, run_peer_loop};
pub use session::{SessionConfig, SyncSession, run_event_loop, run_periodic, to_index_update};
pub use ssh_server::{SshServerHandle, SshServerShared, host_public_key_text, run_ssh_server};
pub use sync::{SyncStats, make_ssh_puller, read_blob_bytes, run_sync, scan_and_index};
pub use trust::{Trust, TrustError};
pub use watcher::{EventKind, FileEvent, watch_folder};
