use std::{
    collections::HashMap,
    sync::{atomic::{AtomicBool, Ordering}, Arc, RwLock},
};

use log::error;

use crate::{
    device::{
        ToHostCtrlRbDesc, ToHostCtrlRbDescQpManagement, ToHostCtrlRbDescSetNetworkParam,
        ToHostCtrlRbDescSetRawPacketReceiveMeta, ToHostCtrlRbDescUpdateMrTable,
        ToHostCtrlRbDescUpdatePageTable, ToHostRb,
    },
    op_ctx::CtrlOpCtx,
    Error,
};

#[derive(Debug)]
pub(crate) struct ControlPoller {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

pub(crate) struct ControlPollerContext {
    pub(crate) to_host_ctrl_rb: Arc<dyn ToHostRb<ToHostCtrlRbDesc>>,
    pub(crate) ctrl_op_ctx_map: Arc<RwLock<HashMap<u32, CtrlOpCtx>>>,
}

unsafe impl Send for ControlPollerContext {}

impl ControlPoller {
    pub(crate) fn new(ctx: ControlPollerContext) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread =
            std::thread::spawn(move || ControlPollerContext::poll_ctrl_thread(&ctx, &thread_stop_flag));
        Self {
            thread: Some(thread),
            stop_flag,
        }
    }
}

impl ControlPollerContext {
    pub(crate) fn poll_ctrl_thread(ctx: &Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            let desc = match ctx.to_host_ctrl_rb.pop() {
                Ok(desc) => desc,
                Err(e) => {
                    error!("failed to fetch descriptor from ctrl rb : {:?}", e);
                    return;
                }
            };

            let result = match desc {
                ToHostCtrlRbDesc::UpdateMrTable(desc) => {
                    ctx.handle_ctrl_desc_update_mr_table(&desc)
                }
                ToHostCtrlRbDesc::UpdatePageTable(desc) => {
                    ctx.handle_ctrl_desc_update_page_table(&desc)
                }
                ToHostCtrlRbDesc::QpManagement(desc) => ctx.handle_ctrl_desc_qp_management(&desc),
                ToHostCtrlRbDesc::SetNetworkParam(desc) => {
                    ctx.handle_ctrl_desc_network_management(&desc)
                }
                ToHostCtrlRbDesc::SetRawPacketReceiveMeta(desc) => {
                    ctx.handle_ctrl_desc_raw_packet_receive_meta(&desc)
                }
            };
            if let Err(e) = result {
                error!("poll ctrl thread : {:?}", e);
            }
        }
    }

    fn handle_ctrl_desc_update_mr_table(
        &self,
        desc: &ToHostCtrlRbDescUpdateMrTable,
    ) -> Result<(), Error> {
        let ctx_map = self
            .ctrl_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("ctrl_op_ctx_map lock"))?;

        if let Some(ctx) = ctx_map.get(&desc.common.op_id) {
            if let Err(e) = ctx.set_result(desc.common.is_success) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("no ctrl cmd ctx found");
        }
        Ok(())
    }

    fn handle_ctrl_desc_update_page_table(
        &self,
        desc: &ToHostCtrlRbDescUpdatePageTable,
    ) -> Result<(), Error> {
        let ctx_map = self
            .ctrl_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("ctrl_op_ctx_map lock"))?;

        if let Some(ctx) = ctx_map.get(&desc.common.op_id) {
            if let Err(e) = ctx.set_result(desc.common.is_success) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("no ctrl cmd ctx found");
        }
        Ok(())
    }

    fn handle_ctrl_desc_qp_management(
        &self,
        desc: &ToHostCtrlRbDescQpManagement,
    ) -> Result<(), Error> {
        let ctx_map = self
            .ctrl_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("ctrl_op_ctx_map lock"))?;

        if let Some(ctx) = ctx_map.get(&desc.common.op_id) {
            if let Err(e) = ctx.set_result(desc.common.is_success) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("no ctrl cmd ctx found");
        }
        Ok(())
    }

    fn handle_ctrl_desc_network_management(
        &self,
        desc: &ToHostCtrlRbDescSetNetworkParam,
    ) -> Result<(), Error> {
        let ctx_map = self
            .ctrl_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("ctrl_op_ctx_map lock"))?;

        if let Some(ctx) = ctx_map.get(&desc.common.op_id) {
            if let Err(e) = ctx.set_result(desc.common.is_success) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("no ctrl cmd ctx found");
        }
        Ok(())
    }

    fn handle_ctrl_desc_raw_packet_receive_meta(
        &self,
        desc: &ToHostCtrlRbDescSetRawPacketReceiveMeta,
    ) -> Result<(), Error> {
        let ctx_map = self
            .ctrl_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("ctrl_op_ctx_map lock"))?;

        if let Some(ctx) = ctx_map.get(&desc.common.op_id) {
            if let Err(e) = ctx.set_result(desc.common.is_success) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("no ctrl cmd ctx found");
        }
        Ok(())
    }
}

impl Drop for ControlPoller {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) =  self.thread.take(){
            if let Err(e) = thread.join(){
                error!("Failed to join the WorkDescPoller thread: {:?}", e);
            }
        }
    }
}