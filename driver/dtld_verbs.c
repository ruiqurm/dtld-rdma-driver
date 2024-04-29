// SPDX-License-Identifier: GPL-2.0 OR Linux-OpenIB
/*
 * Copyright (c) 2016 Mellanox Technologies Ltd. All rights reserved.
 * Copyright (c) 2015 System Fabric Works, Inc. All rights reserved.
 */

#include "asm/io.h"
#include "linux/errno.h"
#include "linux/gfp.h"
#include "linux/panic.h"
#include "linux/printk.h"
#include "linux/slab.h"
#include "linux/vmalloc.h"
#include "rdma/ib_user_ioctl_verbs.h"
#include "rdma/ib_verbs.h"
#include <linux/dma-mapping.h>
#include <net/addrconf.h>
#include <rdma/uverbs_ioctl.h>

#include "dtld.h"
#include "dtld_verbs.h"
#include "dtld-abi.h"

static struct rdma_user_mmap_entry *
dtld_user_mmap_entry_insert(struct dtld_ucontext *uctx, void *address,
                 u32 size, u8 mmap_flag, u64 *mmap_offset)
{
    struct dtld_rdma_user_mmap_entry *entry =
        kzalloc(sizeof(*entry), GFP_KERNEL);
    int ret;

    if (!entry)
        return NULL;

    size = PAGE_ALIGN(size);

    entry->address = (u64)address;
    entry->mmap_flag = mmap_flag;
    entry->length = size;

    ret = rdma_user_mmap_entry_insert(&uctx->ibuc, &entry->rdma_entry,
                      size);
    if (ret) {
        kfree(entry);
        return NULL;
    }

    *mmap_offset = rdma_user_mmap_get_offset(&entry->rdma_entry);

    return &entry->rdma_entry;
}

static int dtld_port_immutable(struct ib_device *dev, u32 port_num,
                               struct ib_port_immutable *immutable)
{
    int err;
    struct ib_port_attr attr;

    // TODO: check if this flag is right
    immutable->core_cap_flags = RDMA_CORE_PORT_IBA_ROCE_UDP_ENCAP;

    err = ib_query_port(dev, port_num, &attr);
    if (err)
        return err;

    immutable->pkey_tbl_len = attr.pkey_tbl_len;
    immutable->gid_tbl_len = attr.gid_tbl_len;
    immutable->max_mad_size = IB_MGMT_MAD_SIZE;

    return 0;
}

static int dtld_query_device(struct ib_device *dev, struct ib_device_attr *attr,
                             struct ib_udata *uhw)
{
    struct dtld_dev *dtld = dtld_from_ibdev(dev);

    if (uhw->inlen || uhw->outlen)
        return -EINVAL;

    *attr = dtld->attr;
    return 0;
}

static int dtld_query_port(struct ib_device *dev, u32 port_num,
                           struct ib_port_attr *attr)
{
    struct dtld_dev *rxe = dtld_from_ibdev(dev);

    /* *attr being zeroed by the caller, avoid zeroing it here */
    *attr = rxe->port.attr;

    // TODO: port state should be changed according to real hardware status, not
    // hardcoded.

    // TODO: need lock here?

    attr->state = IB_PORT_ACTIVE;
    attr->phys_state = IB_PORT_PHYS_STATE_LINK_UP;

    return 0;
}

static enum rdma_link_layer dtld_get_link_layer(struct ib_device *dev,
                                                u32 port_num)
{
    return IB_LINK_LAYER_ETHERNET;
}

static int dtld_alloc_ucontext(struct ib_ucontext *ibuc, struct ib_udata *udata)
{
    struct dtld_dev *rxe = dtld_from_ibdev(ibuc->device);
    struct dtld_ucontext *uc = to_dtld_uc(ibuc);
    struct dtld_uresp_alloc_ctx uresp = {};
    int ret;
    pr_err("csr: %llu",rxe->csr_addr);
    pr_err("csr length:%llu",rxe->csr_length);
    uc->csr_entry = dtld_user_mmap_entry_insert(uc, (void*)rxe->csr_addr, rxe->csr_length, 0, &uresp.csr);
    pr_err("uresp %lld\n",uresp.csr);
    ret = ib_copy_to_udata(udata, &uresp, sizeof(uresp));
	if (ret)
		goto err_put_mmap_entries;
    return 0;

err_put_mmap_entries:
    rdma_user_mmap_entry_remove(uc->csr_entry);
    return ret;
}

static void dtld_dealloc_ucontext(struct ib_ucontext *ibuc)
{
    struct dtld_ucontext *uc = to_dtld_uc(ibuc);
	rdma_user_mmap_entry_remove(uc->csr_entry);
}

int dtld_mmap(struct ib_ucontext *context, struct vm_area_struct *vma)
{
    int err;
    struct rdma_user_mmap_entry *rdma_entry;
    struct dtld_rdma_user_mmap_entry *entry;
    pgprot_t prot;

    rdma_entry = rdma_user_mmap_entry_get_pgoff(context, vma->vm_pgoff);
    if (!rdma_entry){
        pr_err("rdma_user_mmap_entry_get_pgoff failed\n");
        return -EINVAL;
    }
    entry = to_dtld_mmap_entry(rdma_entry);
    prot = pgprot_device(vma->vm_page_prot);
    pr_err("entry->address :%llu\n",entry->address);
    err = rdma_user_mmap_io(context, vma, PFN_DOWN(entry->address),
                            entry->length, prot, rdma_entry);
    rdma_user_mmap_entry_put(rdma_entry);
    return err;
}

void dtld_mmap_free(struct rdma_user_mmap_entry *rdma_entry)
{
	struct dtld_rdma_user_mmap_entry *entry = to_dtld_mmap_entry(rdma_entry);
	kfree(entry);
}

static const struct ib_device_ops dtld_dev_ops = {
    .owner = THIS_MODULE,
    .driver_id = RDMA_DRIVER_UNKNOWN, // TODO: Change this to ourselves' when we
    // have one.
    .uverbs_abi_ver = DTLD_UVERBS_ABI_VERSION,
    .alloc_ucontext = dtld_alloc_ucontext,
    .get_port_immutable = dtld_port_immutable,
    .query_port = dtld_query_port,
    .query_device = dtld_query_device,
    .dealloc_ucontext = dtld_dealloc_ucontext,
    .get_link_layer = dtld_get_link_layer,
    .mmap = dtld_mmap,
    .mmap_free = dtld_mmap_free,

    INIT_RDMA_OBJ_SIZE(ib_ah, dtld_ah, ibah),
    INIT_RDMA_OBJ_SIZE(ib_cq, dtld_cq, ibcq),
    INIT_RDMA_OBJ_SIZE(ib_pd, dtld_pd, ibpd),
    INIT_RDMA_OBJ_SIZE(ib_qp, dtld_qp, ibqp),
    INIT_RDMA_OBJ_SIZE(ib_ucontext, dtld_ucontext, ibuc),
};


int dtld_register_device(struct dtld_dev *dtld, const char *ibdev_name)
{
    int err;
    struct ib_device *dev = &dtld->ib_dev;

    dev->phys_port_cnt = 1;
    dev->num_comp_vectors = num_possible_cpus();

    ib_set_device_ops(dev, &dtld_dev_ops);

    // After running this line, an new entry will show in user cmd: `rdma link
    // show`
    err = ib_register_device(dev, ibdev_name, NULL);
    if (err)
        pr_warn("%s failed with error %d\n", __func__, err);

    return err;
}

void dtld_unregister_device(struct dtld_dev *dtld)
{
    ib_unregister_device(&dtld->ib_dev);
    ib_dealloc_device(&dtld->ib_dev);
}