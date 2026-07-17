// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Bridge from the HiSLIP `Device` abstraction to our `GpibController`.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use super::server::Device;
use crate::backend::GpibBackend;

/// A GPIB instrument addressable at `pad` on the shared bus.
pub struct GpibInstrument {
    ctrl: Arc<Mutex<dyn GpibBackend>>,
    pad: u8,
    /// Upper bound on a single bulk read. Matches the Prologix server's
    /// default so behavior is consistent across front-ends.
    max_read: usize,
}

impl GpibInstrument {
    pub fn new(ctrl: Arc<Mutex<dyn GpibBackend>>, pad: u8) -> Self {
        Self {
            ctrl,
            pad,
            max_read: 65536,
        }
    }
}

#[async_trait::async_trait]
impl Device for GpibInstrument {
    async fn execute(&self, cmd: &[u8], expect_response: bool) -> Result<Option<Vec<u8>>> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.write(self.pad, cmd, true).await?;
        if !expect_response {
            return Ok(None);
        }
        let (data, _eom) = ctrl.read(self.pad, self.max_read).await?;
        Ok(Some(data))
    }

    async fn trigger(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.trigger(self.pad).await
    }

    async fn clear(&self) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.device_clear(self.pad).await
    }

    async fn set_remote(&self, remote: bool) -> Result<()> {
        let mut ctrl = self.ctrl.lock().await;
        ctrl.ren(remote).await
    }
}
