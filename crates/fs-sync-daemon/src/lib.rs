//! fs-sync-daemon：编排层（core + net + store 组合，供桌面与 CLI 共用）。

pub mod engine;
pub mod pairing;
pub mod runtime;
pub mod session;
pub mod sync;
pub mod trust;
pub mod watcher;

pub use engine::{SyncEngine, WriteSuppression, ensure_within};
pub use pairing::{PairingManager, PendingPairing, fingerprint, fingerprint_str};
pub use runtime::{FolderRuntime, run_peer_loop, run_peer_writer};
pub use session::{SessionConfig, SyncSession, run_event_loop, run_periodic, to_index_update};
pub use sync::{SyncStats, make_puller, read_blob_bytes, run_sync, scan_and_index};
pub use trust::{Trust, TrustError};
pub use watcher::{EventKind, FileEvent, watch_folder};
