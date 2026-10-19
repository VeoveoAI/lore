// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
//! Per-instance deterministic barriers for storage publication/cancellation tests.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_storage::StoreError;
use tokio::sync::Notify;

/// A barrier that pauses one operation without a time-based race.
#[derive(Default)]
pub struct Barrier {
    pub pause: AtomicBool,
    pub reached: Notify,
    pub resume: Notify,
}
impl Barrier {
    pub(crate) async fn wait(&self) {
        if self.pause.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.resume.notified().await;
        }
    }
}
/// Synthetic fault controls, never compiled into the production server.
#[derive(Default)]
pub struct TestHooks {
    pub upload: Barrier,
    pub uploaded: Barrier,
    pub delete: Barrier,
    pub retired: Barrier,
    pub fail_delete: AtomicBool,
    pub fail_publish: AtomicBool,
}
impl TestHooks {
    pub(crate) async fn before_upload(&self) {
        self.upload.wait().await;
    }
    pub(crate) async fn after_upload(&self) {
        self.uploaded.wait().await;
    }
    pub(crate) async fn before_delete(&self) -> Result<(), StoreError> {
        self.delete.wait().await;
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(StoreError::internal(
                "Synthetic fixture physical deletion failure",
            ));
        }
        Ok(())
    }
}
