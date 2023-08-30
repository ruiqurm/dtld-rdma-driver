#ifndef LIBXDMA_H
#define LIBXDMA_H

#include <linux/version.h>
#include <linux/types.h>
#include <linux/spinlock_types.h>
#include <linux/list.h>
#include <linux/workqueue.h>
#include <linux/interrupt.h>
#include <linux/dma-direction.h>

#define HAS_SWAKE_UP_ONE (LINUX_VERSION_CODE >= KERNEL_VERSION(4, 19, 0))
#define HAS_SWAKE_UP (LINUX_VERSION_CODE >= KERNEL_VERSION(4, 6, 0))
#if	HAS_SWAKE_UP
#include <linux/swait.h>
#endif


#define XDMA_CHANNEL_NUM_MAX (4)
#define XDMA_BAR_NUM (6)
#define MAGIC_DEVICE	0xDDDDDDDDUL
#define MAX_USER_IRQ 16
#define XDMA_BAR_SIZE (0x8000UL)
#define XDMA_OFS_INT_CTRL	(0x2000UL)


#ifdef __LIBXDMA_DEBUG__
#define dbg_io		pr_err
#define dbg_fops	pr_err
#define dbg_perf	pr_err
#define dbg_sg		pr_err
#define dbg_tfr		pr_err
#define dbg_irq		pr_err
#define dbg_init	pr_err
#define dbg_desc	pr_err
#else
/* disable debugging */
#define dbg_io(...)
#define dbg_fops(...)
#define dbg_perf(...)
#define dbg_sg(...)
#define dbg_tfr(...)
#define dbg_irq(...)
#define dbg_init(...)
#define dbg_desc(...)
#endif

enum dev_capabilities {
	CAP_64BIT_DMA = 2,
	CAP_64BIT_DESC = 4,
	CAP_ENGINE_WRITE = 8,
	CAP_ENGINE_READ = 16
};

enum shutdown_state {
	ENGINE_SHUTDOWN_NONE = 0,	/* No shutdown in progress */
	ENGINE_SHUTDOWN_REQUEST = 1,	/* engine requested to shutdown */
	ENGINE_SHUTDOWN_IDLE = 2	/* engine has shutdown and is idle */
};


/**
 * SG DMA Controller status and control registers
 *
 * These registers make the control interface for DMA transfers.
 *
 * It sits in End Point (FPGA) memory BAR[0] for 32-bit or BAR[0:1] for 64-bit.
 * It references the first descriptor which exists in Root Complex (PC) memory.
 *
 * @note The registers must be accessed using 32-bit (PCI DWORD) read/writes,
 * and their values are in little-endian byte ordering.
 */
struct engine_regs {
	u32 identifier;
	u32 control;
	u32 control_w1s;
	u32 control_w1c;
	u32 reserved_1[12];	/* padding */

	u32 status;
	u32 status_rc;
	u32 completed_desc_count;
	u32 alignments;
	u32 reserved_2[14];	/* padding */

	u32 poll_mode_wb_lo;
	u32 poll_mode_wb_hi;
	u32 interrupt_enable_mask;
	u32 interrupt_enable_mask_w1s;
	u32 interrupt_enable_mask_w1c;
	u32 reserved_3[9];	/* padding */

	u32 perf_ctrl;
	u32 perf_cyc_lo;
	u32 perf_cyc_hi;
	u32 perf_dat_lo;
	u32 perf_dat_hi;
	u32 perf_pnd_lo;
	u32 perf_pnd_hi;
} __packed;

struct interrupt_regs {
	u32 identifier;
	u32 user_int_enable;
	u32 user_int_enable_w1s;
	u32 user_int_enable_w1c;
	u32 channel_int_enable;
	u32 channel_int_enable_w1s;
	u32 channel_int_enable_w1c;
	u32 reserved_1[9];	/* padding */

	u32 user_int_request;
	u32 channel_int_request;
	u32 user_int_pending;
	u32 channel_int_pending;
	u32 reserved_2[12];	/* padding */

	u32 user_msi_vector[8];
	u32 channel_msi_vector[8];
} __packed;

/**
 * Descriptor for a single contiguous memory block transfer.
 *
 * Multiple descriptors are linked by means of the next pointer. An additional
 * extra adjacent number gives the amount of extra contiguous descriptors.
 *
 * The descriptors are in root complex memory, and the bytes in the 32-bit
 * words must be in little-endian byte ordering.
 */
struct xdma_desc {
	u32 control;
	u32 bytes;		/* transfer length in bytes */
	u32 src_addr_lo;	/* source address (low 32-bit) */
	u32 src_addr_hi;	/* source address (high 32-bit) */
	u32 dst_addr_lo;	/* destination address (low 32-bit) */
	u32 dst_addr_hi;	/* destination address (high 32-bit) */
	/*
	 * next descriptor in the single-linked list of descriptors;
	 * this is the PCIe (bus) address of the next descriptor in the
	 * root complex memory
	 */
	u32 next_lo;		/* next desc address (low 32-bit) */
	u32 next_hi;		/* next desc address (high 32-bit) */
} __packed;

/* 32 bytes (four 32-bit words) or 64 bytes (eight 32-bit words) */
struct xdma_result {
	u32 status;
	u32 length;
	u32 reserved_1[6];	/* padding */
} __packed;

struct xdma_dev;

struct xdma_engine {
	unsigned long magic;	/* structure ID for sanity checks */
	struct xdma_dev *xdev;	/* parent device */
	char name[16];		/* name of this engine */
	int version;		/* version of this engine */

	/* HW register address offsets */
	struct engine_regs *regs;		/* Control reg BAR offset */
	// struct engine_sgdma_regs *sgdma_regs;	/* SGDAM reg BAR offset */
	u32 bypass_offset;			/* Bypass mode BAR offset */

	/* Engine state, configuration and flags */
	enum shutdown_state shutdown;	/* engine shutdown mode */
	enum dma_data_direction dir;
	u8 addr_align;		/* source/dest alignment in bytes */
	u8 len_granularity;	/* transfer length multiple */
	u8 addr_bits;		/* HW datapath address width */
	u8 channel:2;		/* engine indices */
	u8 streaming:1;
	u8 device_open:1;	/* flag if engine node open, ST mode only */
	u8 running:1;		/* flag if the driver started engine */
	u8 non_incr_addr:1;	/* flag if non-incremental addressing used */
	u8 eop_flush:1;		/* st c2h only, flush up the data with eop */
	u8 filler:1;

	int max_extra_adj;	/* descriptor prefetch capability */
	int desc_dequeued;	/* num descriptors of completed transfers */
	u32 desc_max;		/* max # descriptors per xfer */
	u32 status;		/* last known status of device */
	/* only used for MSIX mode to store per-engine interrupt mask value */
	u32 interrupt_enable_mask_value;

	/* Transfer list management */
	struct list_head transfer_list;	/* queue of transfers */

	/* Members applicable to AXI-ST C2H (cyclic) transfers */
	struct xdma_result *cyclic_result;
	dma_addr_t cyclic_result_bus;	/* bus addr for transfer */
	u8 *perf_buf_virt;
	dma_addr_t perf_buf_bus; /* bus address */

	/* Members associated with polled mode support */
	u8 *poll_mode_addr_virt;	/* virt addr for descriptor writeback */
	dma_addr_t poll_mode_bus;	/* bus addr for descriptor writeback */

	/* Members associated with interrupt mode support */
#if	HAS_SWAKE_UP
	struct swait_queue_head shutdown_wq;
#else
	wait_queue_head_t shutdown_wq;	/* wait queue for shutdown sync */
#endif
	spinlock_t lock;		/* protects concurrent access */
	int prev_cpu;			/* remember CPU# of (last) locker */
	int msix_irq_line;		/* MSI-X vector for this engine */
	u32 irq_bitmask;		/* IRQ bit mask for this engine */
	struct work_struct work;	/* Work queue for interrupt handling */

	struct mutex desc_lock;		/* protects concurrent access */
	dma_addr_t desc_bus;
	struct xdma_desc *desc;
	int desc_idx;			/* current descriptor index */
	int desc_used;			/* total descriptors used */

	/* for performance test support */
	// struct xdma_performance_ioctl *xdma_perf;	/* perf test control */
// #if	HAS_SWAKE_UP
// 	struct swait_queue_head xdma_perf_wq;
// #else
// 	wait_queue_head_t xdma_perf_wq;	/* Perf test sync */
// #endif

	// struct xdma_kthread *cmplthp;
	/* completion status thread list for the queue */
	// struct list_head cmplthp_list;
	/* pending work thread list */
	/* cpu attached to intr_work */
	unsigned int intr_work_cpu;
};

struct xdma_user_irq {
	struct xdma_dev *xdev;		/* parent device */
	u8 user_idx;			/* 0 ~ 15 */
	u8 events_irq;			/* accumulated IRQs */
	spinlock_t events_lock;		/* lock to safely update events_irq */
	wait_queue_head_t events_wq;	/* wait queue to sync waiting threads */
	irq_handler_t handler;

	void *dev;
};

#define XDEV_FLAG_OFFLINE	0x1
struct xdma_dev {
	struct list_head list_head;
	struct list_head rcu_node;

	unsigned long magic;		/* structure ID for sanity checks */
	struct pci_dev *pdev;	/* pci device struct from probe() */
	int idx;		/* dev index */

	const char *mod_name;		/* name of module owning the dev */

	spinlock_t lock;		/* protects concurrent access */
	unsigned int flags;

	/* PCIe BAR management */
	void __iomem *bar[XDMA_BAR_NUM];	/* addresses for mapped BARs */
	int user_bar_idx;	/* BAR index of user logic */
	int config_bar_idx;	/* BAR index of XDMA config logic */
	int bypass_bar_idx;	/* BAR index of XDMA bypass logic */
	int regions_in_use;	/* flag if dev was in use during probe() */
	int got_regions;	/* flag if probe() obtained the regions */

	int user_max;
	int c2h_channel_max;
	int h2c_channel_max;

	/* Interrupt management */
	int irq_count;		/* interrupt counter */
	int irq_line;		/* flag if irq allocated successfully */
	int msi_enabled;	/* flag if msi was enabled for the device */
	int msix_enabled;	/* flag if msi-x was enabled for the device */
#if KERNEL_VERSION(4, 12, 0) > LINUX_VERSION_CODE
	struct msix_entry entry[32];	/* msi-x vector/entry table */
#endif
	struct xdma_user_irq user_irq[16];	/* user IRQ management */
	unsigned int mask_irq_user;

	/* XDMA engine management */
	int engines_num;	/* Total engine count */
	u32 mask_irq_h2c;
	u32 mask_irq_c2h;
	struct xdma_engine engine_h2c[XDMA_CHANNEL_NUM_MAX];
	struct xdma_engine engine_c2h[XDMA_CHANNEL_NUM_MAX];

	/* SD_Accel specific */
	enum dev_capabilities capabilities;
	u64 feature_id;
};

static inline void xdma_device_flag_set(struct xdma_dev *xdev, unsigned int f)
{
	unsigned long flags;

	spin_lock_irqsave(&xdev->lock, flags);
	xdev->flags |= f;
	spin_unlock_irqrestore(&xdev->lock, flags);
}

static inline void xdma_device_flag_clear(struct xdma_dev *xdev, unsigned int f)
{
	unsigned long flags;

	spin_lock_irqsave(&xdev->lock, flags);
	xdev->flags &= ~f;
	spin_unlock_irqrestore(&xdev->lock, flags);
}

void *xdma_device_open(const char *mname, struct pci_dev *pdev, int *user_max,
		       int *h2c_channel_max, int *c2h_channel_max) __attribute__((used));

#endif