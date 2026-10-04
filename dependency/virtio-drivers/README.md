# 本地 VirtIO 块设备后端

此目录基于上游 `virtio-drivers` 0.11.0，保留原许可证。本项目直接修改这一份库，不维护另一份后端或旧 HAL 兼容入口。

## 当前编译范围

- `alloc` 是 queue、block 与 transport 的明确编译条件；无 `alloc` 只提供平台屏障、设备地址及原始 DMA 视图。
- 设备只编译 `device::blk`；历史 net、GPU、console、socket、sound 等源文件不进入当前实现。
- transport 为真实 MMIO version 1/2 与现代 PCI。PCI 构造只接受已选设备的有界 capability 窗口，不重新扫描 host 或探测 BAR。
- RISC-V64、LoongArch64 的 QEMU 路径由 `os/` 消费。`examples/` 是上游历史源码，仍使用已经删除的接口，不是当前可构建的演示或验收入口；不宣称 AArch64/x86/Hyp 后端支持。

## 准备、发表与停止

1. 内核拥有原控制页与 payload 映射。`DmaAddr`、`DmaRegion` 与 `MappedSegment` 不复制页拥有权。
2. `Hal` 仅提供设备写、读和完整屏障，不分配内存、不隐式 `share/unshare`、不转换 MMIO 地址。
3. `VirtQueue::new(RingLayout, DmaRegion)` 在普通任务、队列锁外 fallible 准备真实深度 shadow；此时不设置硬件 queue。
4. `VirtIOBlk::negotiate` 读取实际容量及限制；unsafe `new` 绑定已经准备的 ring/PDU；`publish` 才设置 queue 与真实 `DRIVER_OK`。
5. unsafe `submit` 使用稳定 tag 的原始 header/status 与已经映射的 payload SG，返回真实 descriptor head。提交不分配、不忙等、不通知；`Notification` 在资源拥有者保活时负责锁外 doorbell。
6. `pop_used` 按真实 head 和可信 shadow 归 descriptor；完成者在真实设备访问结束后解码原始 status，再归还上层访问权。tag 生命周期由请求核心负责，不按完成顺序或 CPU 推断。
7. 停止必须显式请求 reset 并真实读回 status0；写零、软件等待结束或 transport 析构都不是停止证明。库不拥有内核页，也不在 Drop 中忙等或释放仍在 DMA 的资源。
8. 原控制页和 CPU 映射还须覆盖 queue/driver 的全部 CPU 访问期，通常保持至这些对象销毁。资源归还同时要求 CPU 访问结束与设备停止；仅确认 reset 不允许提前释放仍被安全方法访问的内存。

所有原始视图、队列绑定/提交及通知的 unsafe 前提见对应中文 API 文档；coherent 不免除访问互斥或设备屏障。

## 协商边界

仅协商实际使用的 RO、FLUSH、SIZE_MAX、SEG_MAX、BLK_SIZE，现代设备必须协商 VERSION_1 并确认 FEATURES_OK。实际 split-ring depth 不超过设备上限和 128，稳定 PDU tag 不超过 32；descriptor 与 tag 额度分开。

不协商 INDIRECT_DESC、EVENT_IDX、PACKED、MQ、ACCESS_PLATFORM。没有 IOMMU、cache maintenance、隐式全量 bounce、旧 slice 完成入口或自动旧 HAL 后备。

## 已观察验证

native 17 项协议边界/受控 ring 回归通过；受控反序只证明算法，不冒称 QEMU 硬件乱序。两个实际内核使用 `os/Cargo.lock`，native 测试生成的独立宿主锁版本不同。

RV 原 MMIO v1 与 LA PCI 的 2CPU、512/768MiB、双盘初始化已运行：真实 ring/PDU、实际 capacity/limits、DRIVER_OK、分别 ring/PDU 真 OOM、失败时硬件状态未发表 DRIVER_OK、恢复后重新发表、真实 reset0 与完整 MM 页账归还。原文在根 `build/stage4-batch2/`；此处尚不把初始化当读盘、共享 IRQ 数据完成或文件系统验收。

完整接口、固定 Linux 主参考、Asterinas 补充与阶段停止边界见根 `.trellis/spec/os/backend/dma-virtio.md`。当前用户授权还包含后续请求核心和只读文件通路，不能把本目录的初始化验证当整个阶段四完成。
