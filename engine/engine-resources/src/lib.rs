//! Background access to loose and packed opaque resource bytes.
//!
//! [`ResourceSystem`] owns one coordinator and a bounded worker pool. Its
//! cloneable [`ResourceHandle`] only parses virtual paths and submits jobs;
//! filesystem access and archive decoding happen on workers. Requests can be
//! received asynchronously with any executor, polled without blocking, or
//! waited on explicitly by startup and legacy code.

mod error;
mod service;

pub use error::{ResourceError, ResourceErrorKind};
pub use service::{ResourceHandle, ResourceRequest, ResourceStat, ResourceSystem};

use std::path::PathBuf;

/// Settings for the resource coordinator, bounded worker queue, and read budgets.
#[derive(Clone, Debug)]
pub struct ResourceConfig {
    /// Directory containing loose resources and top-level `.rvepak` archives.
    /// Relative paths are interpreted from the process working directory.
    pub root: PathBuf,
    /// Number of synchronous I/O workers. The default is available parallelism,
    /// clamped to one through four workers.
    pub worker_count: usize,
    /// Maximum number of queued jobs. Outstanding requests, including results
    /// not yet received, are separately bounded to this value plus worker count.
    pub queue_capacity: usize,
    /// Aggregate uncompressed archive index bytes loaded at startup.
    pub max_total_index_bytes: u64,
    /// Maximum byte count for one `read_all` or `read_range` result.
    pub max_result_bytes: u64,
    /// Aggregate transient read memory plus completed byte results not yet
    /// received. The reservation transfers to the caller when bytes are received.
    pub max_memory_bytes: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        let worker_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, 4);
        Self {
            root: PathBuf::from("assets"),
            worker_count,
            queue_capacity: 128,
            max_total_index_bytes: 256 * 1024 * 1024,
            max_result_bytes: 256 * 1024 * 1024,
            max_memory_bytes: 512 * 1024 * 1024,
        }
    }
}
