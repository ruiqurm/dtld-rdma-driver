/* SPDX-License-Identifier: ((GPL-2.0 WITH Linux-syscall-note) OR Linux-OpenIB) */
/*
 * Copyright (c) 2016 Mellanox Technologies Ltd. All rights reserved.
 *
 * This software is available to you under a choice of one of two
 * licenses.  You may choose to be licensed under the terms of the GNU
 * General Public License (GPL) Version 2, available from the file
 * COPYING in the main directory of this source tree, or the
 * OpenIB.org BSD license below:
 *
 *     Redistribution and use in source and binary forms, with or
 *     without modification, are permitted provided that the following
 *     conditions are met:
 *
 *	- Redistributions of source code must retain the above
 *	  copyright notice, this list of conditions and the following
 *	  disclaimer.
 *
 *	- Redistributions in binary form must reproduce the above
 *	  copyright notice, this list of conditions and the following
 *	  disclaimer in the documentation and/or other materials
 *	  provided with the distribution.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS
 * BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN
 * ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

#ifndef RDMA_USER_DTLD_H
#define RDMA_USER_DTLD_H

#include <linux/types.h>
#include <linux/socket.h>
#include <linux/in.h>
#include <linux/in6.h>

enum {
	DTLD_NETWORK_TYPE_IPV4 = 1,
	DTLD_NETWORK_TYPE_IPV6 = 2,
};

union dtld_gid {
	__u8	raw[16];
	struct {
		__be64	subnet_prefix;
		__be64	interface_id;
	} global;
};

struct dtld_global_route {
	union dtld_gid	dgid;
	__u32		flow_label;
	__u8		sgid_index;
	__u8		hop_limit;
	__u8		traffic_class;
};

struct dtld_av {
	__u8			port_num;
	/* From DTLD_NETWORK_TYPE_* */
	__u8			network_type;
	__u8			dmac[6];
	struct dtld_global_route	grh;
	union {
		struct sockaddr_in	_sockaddr_in;
		struct sockaddr_in6	_sockaddr_in6;
	} sgid_addr, dgid_addr;
};

struct dtld_send_wr {
	__aligned_u64		wr_id;
	__u32			num_sge;
	__u32			opcode;
	__u32			send_flags;
	union {
		__be32		imm_data;
		__u32		invalidate_rkey;
	} ex;
	union {
		struct {
			__aligned_u64 remote_addr;
			__u32	rkey;
			__u32	reserved;
		} rdma;
		struct {
			__aligned_u64 remote_addr;
			__aligned_u64 compare_add;
			__aligned_u64 swap;
			__u32	rkey;
			__u32	reserved;
		} atomic;
		struct {
			__u32	remote_qpn;
			__u32	remote_qkey;
			__u16	pkey_index;
			__u16	reserved;
			__u32	ah_num;
			__u32	pad[4];
			struct dtld_av av;
		} ud;
		struct {
			__aligned_u64	addr;
			__aligned_u64	length;
			__u32		mr_lkey;
			__u32		mw_rkey;
			__u32		rkey;
			__u32		access;
		} mw;
		/* reg is only used by the kernel and is not part of the uapi */
#ifdef __KERNEL__
		struct {
			union {
				struct ib_mr *mr;
				__aligned_u64 reserved;
			};
			__u32	     key;
			__u32	     access;
		} reg;
#endif
	} wr;
};

struct dtld_sge {
	__aligned_u64 addr;
	__u32	length;
	__u32	lkey;
};

struct mminfo {
	__aligned_u64		offset;
	__u32			size;
	__u32			pad;
};

struct dtld_dma_info {
	__u32			length;
	__u32			resid;
	__u32			cur_sge;
	__u32			num_sge;
	__u32			sge_offset;
	__u32			reserved;
	union {
		__DECLARE_FLEX_ARRAY(__u8, inline_data);
		__DECLARE_FLEX_ARRAY(struct dtld_sge, sge);
	};
};

struct dtld_send_wqe {
	struct dtld_send_wr	wr;
	__u32			status;
	__u32			state;
	__aligned_u64		iova;
	__u32			mask;
	__u32			first_psn;
	__u32			last_psn;
	__u32			ack_length;
	__u32			ssn;
	__u32			has_rd_atomic;
	struct dtld_dma_info	dma;
};

struct dtld_recv_wqe {
	__aligned_u64		wr_id;
	__u32			num_sge;
	__u32			padding;
	struct dtld_dma_info	dma;
};

struct dtld_create_ah_resp {
	__u32 ah_num;
	__u32 reserved;
};

struct dtld_ureq_create_cq {
};

struct dtld_uresp_create_cq {
	__u32 cq_id;
	__u32 num_cqe;
	__aligned_u64 q_offset;
	__aligned_u64 q_length;
};

struct dtld_ureq_create_qp {
	__aligned_u64 db_record_va;
	__aligned_u64 qbuf_va;
	__u32 qbuf_len;
	__u32 rsvd0;
};

struct dtld_uresp_create_qp {
	__u32 qp_id;
	__u32 num_sqe;
	__u32 num_rqe;
	__u32 rq_offset;
};

struct dtld_uresp_alloc_ctx {
	__u32 dev_id;
	__u32 pad;
	__u32 sdb_type;
	__u32 sdb_offset;
	__aligned_u64 sdb;
	__aligned_u64 rdb;
	__aligned_u64 cdb;
};

struct dtld_create_srq_resp {
	struct mminfo mi;
	__u32 srq_num;
	__u32 reserved;
};

struct dtld_modify_srq_cmd {
	__aligned_u64 mmap_info_addr;
};

/* This data structure is stored at the base of work and
 * completion queues shared between user space and kernel space.
 * It contains the producer and consumer indices. Is also
 * contains a copy of the queue size parameters for user space
 * to use but the kernel must use the parameters in the
 * dtld_queue struct. For performance reasons arrange to have
 * producer and consumer indices in separate cache lines
 * the kernel should always mask the indices to avoid accessing
 * memory outside of the data area
 */
struct dtld_queue_buf {
	__u32			log2_elem_size;
	__u32			index_mask;
	__u32			pad_1[30];
	__u32			producer_index;
	__u32			pad_2[31];
	__u32			consumer_index;
	__u32			pad_3[31];
	__u8			data[];
};

#endif /* RDMA_USER_DTLD_H */
