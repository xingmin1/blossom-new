//! 显式 root.mounts 的路径、顺序与挂载拥有测试。
//!
//! 普通用例在系统调用前触发真实的路径或准备错误，不要求挂载权限。
//! 两个 ignored 用例使用 Linux 的独立挂载命名空间执行真实 tmpfs 挂卸载，要求 CAP_SYS_ADMIN；
//! 不具备权限时直接失败，不将未执行的挂载检查记为通过。

use contest_run::model::RootSpec;
use contest_run::runtime::Mounts;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 为每个测试分配独立的临时目录编号。
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 只拥有本用例创建的临时目录；挂载须在析构前通过 Mounts 释放。
struct Scratch(
    /// 本用例独占的临时根目录路径。
    PathBuf,
);

impl Scratch {
    /// 创建带进程号和序号的临时根目录。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "contest-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    /// 构造只含显式挂载列表、没有准备命令的测试根。
    fn root(&self, mounts: &[&str]) -> RootSpec {
        RootSpec {
            path: self.0.clone(),
            cwd: "/".into(),
            env: BTreeMap::new(),
            mounts: mounts.iter().map(|name| (*name).into()).collect(),
            prepare: Vec::new(),
        }
    }
}

impl Drop for Scratch {
    /// 删除已经释放挂载的临时目录树。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 每个显式列表的第一个错误必须原样指出对应路径，shm 只解析为根内的 dev/shm。
#[test]
fn root_mounts_report_first_target_error_in_declared_order() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join("dev")).unwrap();
    fs::write(scratch.0.join("dev/shm"), b"existing shm file").unwrap();
    fs::write(scratch.0.join("tmp"), b"existing tmp file").unwrap();
    fs::write(scratch.0.join("shm"), b"not a shm mount target").unwrap();
    for (names, target) in [(["shm", "tmp"], "dev/shm"), (["tmp", "shm"], "tmp")] {
        let mut mounts = Mounts::default();
        let error = mounts
            .prepare_root(&scratch.root(&names), Duration::from_secs(2))
            .unwrap_err();
        assert!(
            error.starts_with(&format!("mount {}: ", scratch.0.join(target).display())),
            "{error}"
        );
        mounts.release().unwrap();
    }
    assert_eq!(
        fs::read(scratch.0.join("dev/shm")).unwrap(),
        b"existing shm file"
    );
    assert_eq!(
        fs::read(scratch.0.join("tmp")).unwrap(),
        b"existing tmp file"
    );
    assert_eq!(
        fs::read(scratch.0.join("shm")).unwrap(),
        b"not a shm mount target"
    );
}

/// 准备命令的真实非零退出会停止挂载阶段，错误不得变成准备成功。
#[test]
fn root_mounts_prepare_failure_does_not_mount() {
    let scratch = Scratch::new();
    let mut root = scratch.root(&["shm", "tmp"]);
    root.prepare = vec![vec!["/bin/sh".into(), "-c".into(), "exit 7".into()]];
    let mut mounts = Mounts::default();
    let error = mounts
        .prepare_root(&root, Duration::from_secs(2))
        .unwrap_err();
    assert!(error.starts_with("root prepare: "), "{error}");
    assert!(error.contains("code: 7"), "{error}");
    assert!(!scratch.0.join("dev").exists());
    assert!(!scratch.0.join("tmp").exists());
    mounts.release().unwrap();
}

/// 空列表不建立任何隐含的设备或共享内存挂载点。
#[test]
fn root_mounts_empty_list_has_no_implicit_shm() {
    let scratch = Scratch::new();
    let mut mounts = Mounts::default();
    mounts
        .prepare_root(&scratch.root(&[]), Duration::from_secs(2))
        .unwrap();
    assert!(!scratch.0.join("dev").exists());
    assert!(!scratch.0.join("tmp").exists());
    mounts.release().unwrap();
}

/// 为当前测试线程创建私有挂载命名空间，防止测试挂载向父命名空间传播。
fn private_mount_namespace() {
    // SAFETY: unshare 不接收指针；仅为当前测试线程分离挂载命名空间，失败时尚未建立测试挂载。
    let result = unsafe { libc::unshare(libc::CLONE_NEWNS) };
    assert_eq!(result, 0, "unshare: {}", io::Error::last_os_error());
    // SAFETY:
    // 1. 根路径是有效的静态 NUL 结尾字符串，调用期间保持存活；
    // 2. 当前线程已分离挂载命名空间，MS_PRIVATE 仅修改该命名空间的传播属性；
    // 3. MS_REC | MS_PRIVATE 不消费 source、filesystem 或 data，三个空指针符合接口约定。
    let result = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "private mounts: {}", io::Error::last_os_error());
}

/// tmp 先释放，打开的 shm 文件使其自身卸载返回 EBUSY，并保留其父挂载的所有权。
#[test]
#[ignore = "requires Linux CAP_SYS_ADMIN; run the private-namespace command in docs/contest-run.md"]
fn root_mounts_release_reverse_order_and_preserve_busy() {
    private_mount_namespace();
    let scratch = Scratch::new();
    let dev = scratch.0.join("dev");
    let tmp = scratch.0.join("tmp");
    fs::create_dir(&dev).unwrap();
    fs::create_dir(&tmp).unwrap();
    fs::write(dev.join("under-dev"), b"disk dev").unwrap();
    fs::write(tmp.join("under-tmp"), b"disk tmp").unwrap();

    let mut mounts = Mounts::default();
    mounts
        .mount("tmpfs".as_ref(), &dev, "tmpfs", false)
        .unwrap();
    fs::create_dir(dev.join("shm")).unwrap();
    mounts
        .prepare_root(&scratch.root(&["shm", "tmp"]), Duration::from_secs(2))
        .unwrap();
    assert!(!dev.join("under-dev").exists());
    assert!(!tmp.join("under-tmp").exists());
    let shared = dev.join("shm/shared");
    fs::write(&shared, b"shared contents").unwrap();
    let held = File::open(&shared).unwrap();

    let error = mounts.release().unwrap_err();
    assert_eq!(
        error,
        format!(
            "unmount {}: {}",
            dev.join("shm").display(),
            io::Error::from_raw_os_error(libc::EBUSY)
        )
    );
    assert_eq!(fs::read(tmp.join("under-tmp")).unwrap(), b"disk tmp");
    assert!(!dev.join("under-dev").exists());
    assert_eq!(fs::read(&shared).unwrap(), b"shared contents");

    drop(held);
    mounts.release().unwrap();
    assert_eq!(fs::read(dev.join("under-dev")).unwrap(), b"disk dev");
    assert!(!dev.join("shm").exists());
    mounts.release().unwrap();
}

/// shm 已成功挂载后，后续目标路径错误仍返回原错误，已拥有的子挂载和父挂载均可释放。
#[test]
#[ignore = "requires Linux CAP_SYS_ADMIN; run the private-namespace command in docs/contest-run.md"]
fn root_mounts_partial_prepare_failure_can_release_owned_mounts() {
    private_mount_namespace();
    let scratch = Scratch::new();
    let dev = scratch.0.join("dev");
    fs::create_dir(&dev).unwrap();
    fs::write(dev.join("under-dev"), b"disk dev").unwrap();
    fs::write(scratch.0.join("tmp"), b"not a mount directory").unwrap();

    let mut mounts = Mounts::default();
    mounts
        .mount("tmpfs".as_ref(), &dev, "tmpfs", false)
        .unwrap();
    fs::create_dir(dev.join("shm")).unwrap();
    let error = mounts
        .prepare_root(&scratch.root(&["shm", "tmp"]), Duration::from_secs(2))
        .unwrap_err();
    assert!(
        error.starts_with(&format!("mount {}: ", scratch.0.join("tmp").display())),
        "{error}"
    );
    fs::write(dev.join("shm/shared"), b"mounted before failure").unwrap();
    mounts.release().unwrap();
    assert_eq!(fs::read(dev.join("under-dev")).unwrap(), b"disk dev");
    assert!(!dev.join("shm").exists());
    assert_eq!(
        fs::read(scratch.0.join("tmp")).unwrap(),
        b"not a mount directory"
    );
}
