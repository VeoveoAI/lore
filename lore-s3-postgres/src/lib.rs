// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
//! Native S3 payload storage with `PostgreSQL` partition associations and references.

mod database;
mod immutable;
mod mutable;
#[cfg(feature = "test-util")]
pub mod test_util;

pub use database::Database;
pub use database::valid_schema as database_schema_valid;
pub use immutable::S3PostgresStore;
pub use mutable::PostgresMutableStore;
