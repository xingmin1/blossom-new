//! 比赛测试执行器与内置自测的共享库：读取计划配置、按测试根展开用例、控制子进程并做设备通知。
//!
//! 比赛路径不做输出捕获与判题：计划给出分组、用例与 argv，执行层只负责生命周期、超时和进程组收尾，
//! 分组与结果文本由配置自身提供。TAP 解析与自测判定只服务于 `user-selftests` 入口。

/// 各阶段统一命令、显式执行根与独立来源路径。
pub mod command;
/// 计划（TOML）的一层加载与静态边界校验。
pub mod config;
/// 内核测试通知设备（`/dev/testhook`）的单消息写入。
pub mod custom;
/// 单个用例的 before 通知、setup、主命令、after 文本与 cleanup 编排。
pub mod execute;
/// 固定字段的单遍模板替换。
pub mod expand;
/// 配置模型与展开后用例的数据结构。
pub mod model;
/// 子进程退出状态与自测结果判定。
pub mod outcome;
/// 基于 fork/execve、进程组信号控制与单调时钟超时的子进程执行引擎。
pub mod process;
/// 测试根挂载管理、受限路径解析与 ELF 解释器诊断。
pub mod runtime;
/// 把配置按架构、覆盖规则与命令行过滤展开为可执行用例组。
pub mod selection;
/// 内置用户态自测集的解析、筛选与运行。
pub mod selftests;
/// TAP 文本输出流的状态机解析器，`TapObserver` 供 process 与集成测试使用。
pub mod tap;
