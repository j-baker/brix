//! Opaque owning roots shared by cursors, flakes, and schema caches.

pub(crate) type Lease = brix_eval::managed_api::GcValue;
