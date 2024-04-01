use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
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
    _thread: std::thread::JoinHandle<()>,
}

pub(crate) struct ControlPollerContext {
    pub(crate) to_host_ctrl_rb: Arc<dyn ToHostRb<ToHostCtrlRbDesc>>,
    pub(crate) ctrl_op_ctx_map: Arc<RwLock<HashMap<u32, CtrlOpCtx>>>,
}

unsafe impl Send for ControlPollerContext {}

impl ControlPoller {
    pub(crate) fn new(ctx: ControlPollerContext) -> Self {
        let thread = std::thread::spawn(move || ControlPollerContext::poll_ctrl_thread(&ctx));
        Self { _thread: thread }
    }
}

impl ControlPollerContext {
    pub(crate) fn poll_ctrl_thread(ctx: &Self) {
        loop {
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
