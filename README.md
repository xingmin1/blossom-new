## OSKernel2025-NPUcore-BLOSSOM (RISC-V + LoongArch)

### 一、简介

`NPUcore-BLOSSOM` 是来自西北工业大学的三位同学基于 `NPUcore-lwext4` 框架，参考借鉴去年内核赛道优秀参赛队伍与 `Linux` 内核的诸多优秀设计，完善其内部功能实现并进行迭代升级而形成的竞赛操作系统。

---

### 二、决赛完成情况

#### （1）决赛阶段 1 测试得分表

| 测试点         | glibc-la        | glibc-rv        | musl-la         | musl-rv         | 总分           |
| -------------- | --------------- | --------------- | --------------- | --------------- | -------------- |
| copyfilerange  | 7.5             | 7.5             | 7.5             | 7.5             | 30.0           |
| interrupts     | 3.75            | 3.75            | 3.75            | 3.75            | 15.0           |
| splice         | 10              | 10              | 10              | 10              | 40             |
| **总分** | **21.25** | **21.25** | **21.25** | **21.25** | **85.0** |

#### （2）决赛阶段初赛重测 测试得分表

| 测试点         | glibc-la                     | glibc-rv                    | musl-la                     | musl-rv                     | 总分                        |
| -------------- | ---------------------------- | --------------------------- | --------------------------- | --------------------------- | --------------------------- |
| basic          | 102                          | 102                         | 102                         | 102                         | 408                         |
| busybox        | 49                           | 48                          | 54                          | 53                          | 204                         |
| cyclictest     | 0.0                          | 0.0                         | 0.0                         | 0.0                         | 0.0                         |
| iozone         | 0.0                          | 0.0                         | 0.0                         | 0.0                         | 0.0                         |
| iperf          | 0.0                          | 0.0                         | 0.0                         | 0.0                         | 0.0                         |
| libcbench      | 0.0                          | 0.0                         | 0.0                         | 0.0                         | 0.0                         |
| libctest       | -                            | -                           | 185                         | 185                         | 370                         |
| lmbench        | 35.539892441278674           | 36.2748649755899            | 26.954781536985646          | 28.41534870827281           | 127.18488766212704          |
| ltp            | 0                            | 0                           | 316                         | 319                         | 635                         |
| lua            | 9                            | 9                           | 9                           | 9                           | 36                          |
| netperf        | 0.0                          | 0.0                         | 0.0                         | 0.0                         | 0.0                         |
| **总分** | **195.53989244127868** | **195.2748649755899** | **692.9547815369856** | **696.4153487082729** | **1780.184887662127** |

----

### 三、分支介绍

- **`ForFianl` —— 决赛线上测例及上板分支**
- `RvLaMerge` ——初赛仓库分支
- `la_ext4_init` ——支持 EXT4 文件系统的 LoongArch 分支
- `main` —— RISC-V 版本的 baseline
- `ltp` —— 适配 ltp 测例的分支

---

### 四、文档

设计文档为本仓库的**决赛设计文档.pdf**，面对以下仓库的内容进行了简单介绍
- **仓库地址：** https://gitee.com/differential1012/npucore-blossom-docs

---

### 五、Demo

- [初赛测评demo](https://pan.baidu.com/s/1n96Q_IbJ-VjQ0bWSVBW3hA?pwd=qcre)

- [拓展功能demo](https://pan.baidu.com/s/1V1t0xwEsobjb84zbqir-Ng?pwd=vqxp)

### 六、参考资料

- baseline

RISC-V：[oskernel2023-npucore-plus](https://gitlab.eduxiji.net/202310699101073/oskernel2023-npucore-plus/-/tree/master?ref_type=heads)

LoongArch：[NPUcore-IMPACT](https://github.com/Fediory/NPUcore-IMPACT)

- dependency

[rustsbi](https://github.com/rustsbi/rustsbi)
[virtio-drivers](https://github.com/rcore-os/virtio-drivers)