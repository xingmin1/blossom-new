//! 显式命令根的配置、选择与真实进程消费者回归。
//!
//! 普通用例只使用独占临时目录，不挂比赛盘、不访问通知设备，也不改真实运行库。
//! CLI 预览用例需要显式提供可写的临时 ext4 块设备及 CAP_SYS_ADMIN，默认忽略。
//! 整个分组的执行频率和停止策略由客体执行器验证；这里不另造分组执行实现。

use contest_run::command::{CommandSpec, Root};
use contest_run::execute::execute_case;
use contest_run::model::Config;
use contest_run::outcome::ExitOutcome;
use contest_run::runtime::Mounts;
use contest_run::{config, selection};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 用配置解码器读取单条命令，保留与计划字段相同的反序列化边界。
#[derive(Deserialize)]
struct Holder {
    /// 待解析的主命令。
    exec: CommandSpec,
}

/// 从最小 TOML 文档中取出命令配置。
fn command(text: &str) -> CommandSpec {
    toml::from_str::<Holder>(text).unwrap().exec
}

/// 提供与系统工具不兼容的测试根默认环境，便于观察是否错误继承。
fn environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("LD_LIBRARY_PATH".into(), "/test-only/lib".into()),
        ("ROOT_ONLY".into(), "root value".into()),
    ])
}

/// 拥有本用例的计划、来源文件与子进程输出。
struct Fixture(
    /// 测试结束后整体删除的独占目录。
    PathBuf,
);

impl Fixture {
    /// 建立唯一临时目录，不复用前一用例的文件。
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "contest-command-roots-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    /// 返回夹具内路径。
    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// 写入配置或数据文件，父目录由本夹具拥有。
    fn put(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// 创建供 directory 来源读取的普通可执行文件。
    fn executable(&self, relative: &str) {
        self.put(relative, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(self.path(relative), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 通过正式加载器解析当前用例的计划。
    fn load(&self, text: &str) -> Result<Config, String> {
        self.put("plan.toml", text);
        config::load(&self.path("plan.toml"), &[])
    }
}

impl Drop for Fixture {
    /// 删除本用例创建的全部输入和输出。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 数组简写恒取 test 根；完整 test 命令仅覆盖自身 cwd/env/timeout。
#[test]
fn test_commands_keep_defaults_and_merge_local_values() {
    let fixture = Fixture::new();
    let env = environment();
    let short = command("exec = [\"/bin/true\", \"/unrelated/system/path\"]");
    assert_eq!(short.root(), Root::Test);
    let invocation = short.resolve(
        Some(&fixture.0),
        Path::new("/test-cwd"),
        &env,
        Duration::from_secs(60),
    );
    assert_eq!(invocation.root, Some(fixture.0.as_path()));
    assert_eq!(invocation.cwd, Path::new("/test-cwd"));
    assert_eq!(invocation.env.as_ref(), &env);
    assert_eq!(invocation.timeout, Duration::from_secs(60));
    assert_eq!(invocation.argv, ["/bin/true", "/unrelated/system/path"]);

    let full = command(&format!(
        r#"exec = {{ root = "test", cwd = {:?}, env = {{ ROOT_ONLY = "local value", LOCAL = "own" }}, timeout_sec = 2, argv = ["/bin/sh", "-c", "printf '%s\\n' \"$ROOT_ONLY\" \"$LOCAL\" \"$LD_LIBRARY_PATH\" > observed"] }}"#,
        fixture.0
    ));
    let invocation = full.resolve(None, Path::new("/missing"), &env, Duration::from_secs(60));
    assert_eq!(invocation.cwd, fixture.0.as_path());
    assert_eq!(invocation.env["ROOT_ONLY"], "local value");
    assert_eq!(invocation.env["LOCAL"], "own");
    assert_eq!(invocation.env["LD_LIBRARY_PATH"], "/test-only/lib");
    assert_eq!(invocation.timeout, Duration::from_secs(2));
    let run = invocation.run();
    assert!(
        matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }),
        "{run:?}"
    );
    assert!(run.io_errors.is_empty(), "{run:?}");
    assert_eq!(
        fs::read(fixture.path("observed")).unwrap(),
        b"local value\nown\n/test-only/lib\n"
    );
}

/// system 命令真实运行于系统根且不继承测试环境；同一命令切回 test 时保留换根失败。
#[test]
fn system_command_really_avoids_test_chroot_and_environment() {
    let fixture = Fixture::new();
    let env = environment();
    let spec = command(&format!(
        r#"exec = {{ root = "system", argv = ["/bin/sh", "-c", "test \"$PWD\" = / && test -z \"${{LD_LIBRARY_PATH+x}}\" && test -z \"${{ROOT_ONLY+x}}\" && printf '%s' \"$1\" > \"$2\"", "scope", "literal /test/path", {:?}] }}"#,
        fixture.path("system-output")
    ));
    let missing_root = fixture.path("missing-root");
    let invocation = spec.resolve(
        Some(&missing_root),
        Path::new("/missing-cwd"),
        &env,
        Duration::from_secs(2),
    );
    assert_eq!(invocation.root, None);
    assert_eq!(invocation.cwd, Path::new("/"));
    assert!(invocation.env.is_empty());
    let run = invocation.run();
    assert!(
        matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }),
        "{run:?}"
    );
    assert!(run.io_errors.is_empty(), "{run:?}");
    assert_eq!(
        fs::read(fixture.path("system-output")).unwrap(),
        b"literal /test/path"
    );

    fs::remove_file(fixture.path("system-output")).unwrap();
    let test = CommandSpec::Argv(spec.argv().to_vec());
    let run = test
        .resolve(
            Some(&missing_root),
            Path::new("/"),
            &env,
            Duration::from_secs(2),
        )
        .run();
    assert!(
        matches!(run.outcome, ExitOutcome::SpawnError { .. }),
        "{run:?}"
    );
    assert!(!fixture.path("system-output").exists());
}

/// 完整命令必须写有效 root，未知字段在解码时拒绝。
#[test]
fn full_form_requires_explicit_root() {
    for text in [
        r#"exec = { argv = ["/bin/true"] }"#,
        r#"exec = { root = "host", argv = ["/bin/true"] }"#,
        r#"exec = { root = "system", argv = ["/bin/true"], typo = true }"#,
    ] {
        assert!(toml::from_str::<Holder>(text).is_err(), "{text}");
    }
}

/// 所有命令消费者都拒绝完整形式的无效 cwd、环境、时限及 argv，不能绕过统一静态校验。
#[test]
fn full_command_validation_is_shared_by_every_phase() {
    let fixture = Fixture::new();
    for invalid in [
        r#"{ root = "system", argv = [] }"#,
        r#"{ root = "system", argv = [""] }"#,
        r#"{ root = "system", argv = ["/bin/true", "\u0000"] }"#,
        r#"{ root = "system", argv = ["/bin/true"], cwd = "relative" }"#,
        r#"{ root = "system", argv = ["/bin/true"], cwd = "/../escape" }"#,
        r#"{ root = "system", argv = ["/bin/true"], env = { "BAD=KEY" = "x" } }"#,
        r#"{ root = "system", argv = ["/bin/true"], timeout_sec = 0 }"#,
    ] {
        for phase in [
            "prepare",
            "group.setup",
            "group.cleanup",
            "exec",
            "setup",
            "cleanup",
            "override.exec",
            "override.setup",
            "override.cleanup",
        ] {
            let root_field = if phase == "prepare" {
                format!("prepare = [{invalid}]\n")
            } else {
                String::new()
            };
            let group_field = match phase {
                "group.setup" => format!("setup = [{invalid}]\n"),
                "group.cleanup" => format!("cleanup = [{invalid}]\n"),
                _ => String::new(),
            };
            let step_field = match phase {
                "exec" => format!("exec = {invalid}\n"),
                "setup" | "cleanup" => format!("exec = [\"/bin/true\"]\n{phase} = [{invalid}]\n"),
                _ => "exec = [\"/bin/true\"]\n".into(),
            };
            let overrides = if let Some(field) = phase.strip_prefix("override.") {
                let value = if field == "exec" {
                    invalid.to_owned()
                } else {
                    format!("[{invalid}]")
                };
                fixture.put(
                    "override.toml",
                    &format!("[[override]]\nid = \"g:musl:one\"\n{field} = {value}\n"),
                );
                "override_files = [\"override.toml\"]\n"
            } else {
                ""
            };
            let text = format!(
                "{overrides}[roots.musl]\npath = {:?}\n{root_field}[[group]]\nname = \"g\"\nroots = [\"musl\"]\n{group_field}[[group.step]]\nnames = [\"one\"]\n{step_field}",
                fixture.path("musl")
            );
            let error = fixture.load(&text).unwrap_err();
            assert!(
                error.contains("plan.toml") || error.contains("override.toml"),
                "{phase}: {error}"
            );
        }
    }
}

/// 旧 prepare 裸数组必须产生带根位置的迁移错误；两种显式根都可以加载。
#[test]
fn root_prepare_requires_migration_not_silent_semantics_change() {
    let fixture = Fixture::new();
    for (prepare, accepted) in [
        (r#"[["/bin/true"]]"#, false),
        (r#"[{ root = "system", argv = ["/bin/true"] }]"#, true),
        (r#"[{ root = "test", argv = ["/bin/true"] }]"#, true),
    ] {
        let result = fixture.load(&format!(
            "[roots.musl]\npath = {:?}\nprepare = {prepare}\n[[group]]\nname = \"g\"\nroots = [\"musl\"]\n[[group.step]]\nnames = [\"one\"]\nexec = [\"/bin/true\"]\n",
            fixture.path("musl")
        ));
        if accepted {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(
                error.contains("roots.musl") && error.contains("prepare must use"),
                "{error}"
            );
            assert!(
                error.contains("system") && error.contains("argv"),
                "{error}"
            );
        }
    }
}

/// 清单原文、命令 cwd/env 及跨根参数只展开一次，真实 setup、主命令和 cleanup 都保留插入值中的占位符。
#[test]
fn expansion_reaches_processes_once_without_splitting_or_path_guessing() {
    let fixture = Fixture::new();
    let test_root = fixture.path("test root {name}");
    fs::create_dir(&test_root).unwrap();
    let payload = "printf '%s\\n' '{name}' \"$PAYLOAD\" \"$1\" > main";
    fixture.put("catalog", &format!("one {payload}\n"));
    let config = fixture.load(&format!(
        r#"[roots.musl]
path = {test_root:?}
cwd = "/missing-test-cwd"
env = {{ LD_LIBRARY_PATH = "/test/lib" }}
[[group]]
name = "once"
roots = ["musl"]
[[group.step]]
catalog = {{ root = "system", path = {:?} }}
exec = {{ root = "system", cwd = "{{test_root}}", env = {{ PAYLOAD = "{{command}}" }}, argv = ["/bin/sh", "-c", "{{command}}", "main", "{{test_root}}"] }}
setup = [{{ root = "system", cwd = "{{test_root}}", env = {{ PAYLOAD = "{{command}}" }}, argv = ["/bin/sh", "-c", "printf '%s\\n' \"$1\" \"$PAYLOAD\" > setup", "setup", "{{test_root}}"] }}]
cleanup = [{{ root = "system", cwd = "{{test_root}}", argv = ["/bin/sh", "-c", "printf '%s' \"$1\" > cleanup", "cleanup", "{{test_root}}"] }}]
"#,
        fixture.path("catalog")
    )).unwrap();
    let selected = selection::resolve(&config, "riscv64", &[]).unwrap();
    let case = &selected.groups[0].cases[0];
    let mut notifications = Vec::new();
    let record = execute_case(
        Some(&test_root),
        case,
        Duration::from_secs(2),
        &mut Vec::new(),
        |before, id| {
            notifications.push((before, id.to_owned()));
            Ok(())
        },
    );
    assert!(!record.failed() && !record.must_stop(), "{record:?}");
    assert_eq!(
        notifications,
        [
            (true, "once:musl:one".to_owned()),
            (false, "once:musl:one".to_owned())
        ]
    );
    assert_eq!(
        fs::read_to_string(test_root.join("setup")).unwrap(),
        format!("{}\n{payload}\n", test_root.display())
    );
    assert_eq!(
        fs::read_to_string(test_root.join("main")).unwrap(),
        format!("{{name}}\n{payload}\n{}\n", test_root.display())
    );
    assert_eq!(
        fs::read_to_string(test_root.join("cleanup")).unwrap(),
        test_root.to_str().unwrap()
    );
}

/// directory/catalog 的读取根与执行根独立；system 主命令消费实际选出的名字，test 主命令保留目标根失败。
#[test]
#[expect(
    clippy::literal_string_with_formatting_args,
    reason = "TOML 命令中的 {{name}} 经 format! 转义后是执行器模板，测试必须保留字面占位符"
)]
fn source_root_and_execution_root_are_independent() {
    let fixture = Fixture::new();
    fixture.executable("musl/cases/from-test");
    fixture.executable("system/cases/from-system");
    fixture.put("musl/catalog", "from-test ignored-test-command\n");
    fixture.put("system/catalog", "from-system ignored-system-command\n");
    let test_root = fixture.path("musl");
    for (kind, source_root, source, name) in [
        ("directory", "test", PathBuf::from("/cases"), "from-test"),
        (
            "directory",
            "system",
            fixture.path("system/cases"),
            "from-system",
        ),
        ("catalog", "test", PathBuf::from("/catalog"), "from-test"),
        (
            "catalog",
            "system",
            fixture.path("system/catalog"),
            "from-system",
        ),
    ] {
        for execution_root in ["system", "test"] {
            let marker = fixture.path(&format!("{kind}-{source_root}-{execution_root}"));
            let config = fixture.load(&format!(
                r#"[roots.musl]
path = {test_root:?}
cwd = "/unavailable-test-cwd"
[[group]]
name = "source"
roots = ["musl"]
[[group.step]]
{kind} = {{ root = "{source_root}", path = {source:?} }}
exec = {{ root = "{execution_root}", cwd = "/", argv = ["/bin/sh", "-c", "printf '%s' \"$1\" > \"$2\"", "selected", "{{name}}", {marker:?}] }}
"#
            )).unwrap();
            let selected = selection::resolve(&config, "riscv64", &[]).unwrap();
            let case = &selected.groups[0].cases[0];
            assert_eq!(case.root, "musl");
            assert_eq!(case.name, name);
            assert_eq!(case.source.catalog.as_deref(), Some(source.as_path()));
            let record = execute_case(
                Some(&test_root),
                case,
                Duration::from_secs(2),
                &mut Vec::new(),
                |_, _| Ok(()),
            );
            if execution_root == "system" {
                assert!(!record.failed() && !record.must_stop(), "{record:?}");
                assert_eq!(fs::read_to_string(&marker).unwrap(), name);
            } else {
                assert!(
                    matches!(
                        record.execution.as_ref().unwrap().outcome,
                        ExitOutcome::SpawnError { .. }
                    ),
                    "{record:?}"
                );
                assert!(!marker.exists());
            }
        }
    }
}

/// 分离当前测试线程的挂载视图，并禁止测试挂载向父命名空间传播。
fn private_mount_namespace() {
    // SAFETY: unshare 只消费有效的 CLONE_NEWNS 常量，不接收指针；尚未建立测试挂载。
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        0,
        "unshare: {}",
        io::Error::last_os_error()
    );
    // SAFETY:
    // 1. 根路径使用有效的静态 NUL 结尾字符串；
    // 2. 当前线程已持有私有挂载命名空间，递归更改传播属性不影响父命名空间；
    // 3. MS_PRIVATE 不读取 source、filesystem 或 data，三个空指针符合接口约定。
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

/// 实际 CLI 预览展示有效根/cwd/env/argv/timeout，主命令 CLI 时限覆盖完整命令时限且不改变准备命令。
///
/// CONTEST_RUN_TEST_DEVICE 必须是专用于本测试、可写且未挂载的临时 ext4 块设备；不能传入比赛原盘。
#[test]
#[ignore = "requires Linux CAP_SYS_ADMIN and CONTEST_RUN_TEST_DEVICE pointing to a disposable ext4 block device"]
fn cli_list_applies_timeout_to_detailed_main_only_and_runs_no_commands() {
    let device = std::env::var_os("CONTEST_RUN_TEST_DEVICE")
        .expect("set CONTEST_RUN_TEST_DEVICE to a disposable ext4 block device");
    private_mount_namespace();
    let mut mounts = Mounts::default();
    mounts
        .mount(Path::new("tmpfs"), Path::new("/mnt"), "tmpfs", false)
        .unwrap();
    let fixture = Fixture::new();
    let marker = fixture.path("must-not-exist");
    let action = "printf executed > \"$1\"";
    fixture.put("plan.toml", &format!(
        r#"[defaults]
timeout_sec = 17
[roots.musl]
path = {:?}
cwd = "/test-cwd"
env = {{ ROOT_ONLY = "root", LD_LIBRARY_PATH = "/test/lib" }}
prepare = [{{ root = "system", argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}], timeout_sec = 3 }}]
[roots.glibc]
path = {:?}
[[group]]
name = "preview"
roots = ["musl", "glibc"]
cwd = "/group-cwd"
env = {{ GROUP_ONLY = "group" }}
setup = [{{ root = "system", argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}], timeout_sec = 4 }}]
cleanup = [{{ root = "test", argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}], timeout_sec = 7 }}]
[[group.step]]
names = ["one"]
cwd = "/case-cwd"
env = {{ STEP_ONLY = "step" }}
timeout_sec = 11
exec = {{ root = "system", cwd = "/", env = {{ MAIN_ONLY = "main" }}, argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}, "a b"], timeout_sec = 2 }}
setup = [{{ root = "test", argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}], timeout_sec = 5 }}]
cleanup = [{{ root = "system", argv = ["/bin/sh", "-c", {action:?}, "preview", {marker:?}], timeout_sec = 6 }}]
"#,
        fixture.path("musl"), fixture.path("glibc")
    ));
    let test_root = fixture.path("musl");
    for cli_timeout in [None, Some(9)] {
        let mut cli = Command::new(env!("CARGO_BIN_EXE_contest-run"));
        cli.arg(&device)
            .arg("--plan")
            .arg(fixture.path("plan.toml"))
            .args(["--list", "--only", "preview:musl:one"]);
        if let Some(timeout) = cli_timeout {
            cli.args(["--timeout-sec", &timeout.to_string()]);
        }
        let output = cli.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(&format!(
                "GROUP preview / musl path={}",
                fixture.path("musl").display()
            )),
            "{text}"
        );
        assert!(!text.contains("GROUP preview / glibc"), "{text}");
        for (label, scope, cwd, timeout) in [
            ("root.prepare", "system", "/", 3),
            ("group.setup", "system", "/", 4),
            ("setup", "test", "/case-cwd", 5),
            ("exec", "system", "/", cli_timeout.unwrap_or(2)),
            ("cleanup", "system", "/", 6),
            ("group.cleanup", "test", "/group-cwd", 7),
        ] {
            let line = text
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{label} ")))
                .unwrap_or_else(|| panic!("missing {label}: {text}"));
            let path = if scope == "system" {
                Path::new("/")
            } else {
                test_root.as_path()
            };
            let fields = format!(" {line} ");
            for field in [
                format!("root={scope}"),
                format!("path={}", path.display()),
                format!("cwd={cwd}"),
                format!("timeout={timeout}s"),
            ] {
                assert!(
                    fields.contains(&format!(" {field} ")),
                    "missing {field}: {line}"
                );
            }
            let mut argv = vec!["/bin/sh", "-c", action, "preview", marker.to_str().unwrap()];
            if label == "exec" {
                argv.push("a b");
            }
            assert!(line.contains(&format!("argv={argv:?}")), "{line}");
            if label == "exec" {
                assert!(
                    line.contains("\"a b\"") && line.contains("env={\"MAIN_ONLY\": \"main\"}"),
                    "{line}"
                );
            } else if scope == "system" {
                assert!(line.contains("env={}"), "{line}");
            } else {
                assert!(
                    line.contains("\"ROOT_ONLY\": \"root\"")
                        && line.contains("\"GROUP_ONLY\": \"group\""),
                    "{line}"
                );
                assert_eq!(
                    line.contains("\"STEP_ONLY\": \"step\""),
                    label == "setup",
                    "{line}"
                );
            }
        }
        assert!(!marker.exists());
    }
    mounts.release().unwrap();
}
