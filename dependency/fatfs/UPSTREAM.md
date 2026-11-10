# rust-fatfs 上游来源

- 上游仓库：<https://github.com/rafalh/rust-fatfs>
- 固定提交：`38b3a7950bf0e765e34286d1dfe76c519ee347e2`
- 来源归档：<https://codeload.github.com/rafalh/rust-fatfs/tar.gz/38b3a7950bf0e765e34286d1dfe76c519ee347e2>
- 归档 SHA-256：`8a6dbeea33a317d764e9a940f37746e92d5df49bd81368f58645c395f9d9e9d5`
- 许可证：MIT，见上游 `LICENSE.txt`。
- 本地保留上游 `Cargo.toml`、`src/`、`LICENSE.txt`、`README.md` 和 `CHANGELOG.md`；除下列 `src/fs.rs` 变更外，内容未修改。
- 未保留上游 `resources/`、`tests/`、`examples/`、`scripts/`、`.github/`、`build-nostd.sh`、`.editorconfig`、`.gitignore` 和 `rustfmt.toml`。
- 本地新增本来源记录；`os/Cargo.toml` 通过本地路径引用此库。

## 本地变更

- 固定上游已经提供 `FileSystem::flush(&self)`，继续使用原实现，不新增同义同步接口。
- `src/fs.rs` 仅追加 `FileSystem::finish_unmount(&self)`：先调用原有 `unmount_internal()` 更新 FSInfo 与卸载状态，再调用实际存储的 `flush()`，传播任一步骤的原始错误。
- 新接口不消费 `FileSystem`。卸载同步失败时，挂载层仍持有原对象和设备占用；它不回滚已经完成的磁盘写入，也不替代挂载层的 Busy 检查与并发准入控制。
- 只读卷由内核挂载层跳过主动同步；字节适配器及只读 `FsClaim` 拒绝写入，但 `FsClaim` 仍允许设备支持的 `Flush`。FSInfo 空闲数未知时，统计会更新上游内存簿记，其析构写入仍受只读占用限制；原上游析构行为保持不变。
