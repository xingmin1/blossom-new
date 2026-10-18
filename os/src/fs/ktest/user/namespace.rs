//! 名字空间消费者的共同视图、目录块读取与行为 suite 锚点。

use alloc::sync::Arc;

use crate::fs::ext4::{BlockMapping, Ext4Volume};
use crate::fs::ktest::user::write::WriteOwner;
use crate::fs::ktest::{checked, TestContext, TestError};
use crate::fs::{FsContext, FsError, InodeMetadata, Path, ResolveMode, Vfs};
use crate::mm;

mod directory;
mod loading;
mod metadata;

struct NamespaceView {
    vfs: Arc<Vfs>,
    context: FsContext,
    root: Path,
    source: Path,
    volume: Arc<Ext4Volume>,
}

/// 沿 E 提供的原 RW UserOwner/PathOwner 取得本例实际 Vfs、FsContext 与 Volume。
fn prepare_namespace_view(
    context: &TestContext<'_>,
    fixture: &mut WriteOwner,
) -> Result<NamespaceView, TestError> {
    fixture.prepare_namespace(context)?;
    let vfs = fixture.user.path.vfs().clone();
    let fs_context = fixture.user.path.context().clone();
    let root = vfs.root();
    let source = checked(
        context,
        vfs.resolve(&fs_context, b"/pattern", ResolveMode::Follow),
    )?;
    Ok(NamespaceView {
        vfs,
        context: fs_context,
        root,
        source,
        volume: fixture.volume(),
    })
}

/// 以真实 inode 映射取得目录逻辑块；每个 BufferRef 借用结束后才读取下一块。
fn directory_block(
    volume: &Ext4Volume,
    metadata: &InodeMetadata,
    map_root: &[u8; 60],
    logical: u64,
) -> Result<[u8; mm::PAGE_SIZE], FsError> {
    let physical = match volume.map_block(metadata, map_root, logical, None)? {
        BlockMapping::Mapped { physical, .. } => physical,
        _ => return Err(FsError::Corrupt),
    };
    let buffer = volume.metadata_block(physical)?;
    let mut bytes = [0u8; mm::PAGE_SIZE];
    buffer
        .with_read(|data| bytes[..data.len()].copy_from_slice(data))
        .map_err(FsError::from)?;
    Ok(bytes)
}

/// 保活三个真实行为 suite 的注册槽，父入口仍只借一个正式槽。
pub(super) fn anchor() -> &'static ::ktest::SuiteSlot {
    core::hint::black_box([
        metadata::cases::__ktest_anchor(),
        directory::cases::__ktest_anchor(),
        loading::cases::__ktest_anchor(),
    ]);
    metadata::cases::__ktest_anchor()
}
