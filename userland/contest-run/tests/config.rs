use contest_run::config;
use contest_run::model::{CommandSpec, Protocol};
use contest_run::selection;
use std::fmt::Write;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    /// 每例拥有独立临时目录，不读取开发者比赛盘。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "contest-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// 返回测试树内的实际文件路径。
    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// 建立真实清单和计划文件，文件内容通过生产代码读取。
    fn put(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// 可执行目录使用真实脚本和权限，数据文件保持不可执行。
    fn executable(&self, relative: &str) {
        self.put(relative, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(self.path(relative), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 两个名称不同的 root 共享内容，便于核对显式 roots 顺序。
    fn roots(&self) -> String {
        format!(
            "[roots.a]\npath = {:?}\ncwd = '/'\n[roots.b]\npath = {:?}\ncwd = '/'\n",
            self.path("root"),
            self.path("root")
        )
    }

    /// 从同一生产入口加载并展开指定计划。
    fn resolve(&self, plan: &str) -> Result<Vec<contest_run::model::ResolvedCase>, String> {
        let config = config::load(&self.path(plan), &[])?;
        selection::resolve(&config, "riscv64")
    }
}

impl Drop for Tree {
    /// 仅删除本例临时输入，不触及仓库构建或报告目录。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 多文件、roots、include、标准 glob 与 catalog 的顺序分别得到保留。
#[test]
fn multifile_order_globs_exclude_and_locations() {
    let tree = Tree::new();
    tree.put(
        "root/catalog",
        "# comment\ncase2 printf two\ncase1 printf one\ncase3 printf three\nbeta printf beta\n",
    );
    tree.put(
        "plan.toml",
        &format!(
            "files = ['sets/first.toml', 'sets/last.toml']\n{}",
            tree.roots()
        ),
    );
    tree.put("sets/first.toml", "# 多行 include 与注释使用 TOML 原语法\n[[step]]\nkind='ltp'\nroots=['b', 'a']\nsuite='syscalls'\ncatalog='/catalog'\ninclude=[\n 'beta', # 先精确名\n 'case[12]',\n 'case?',\n]\nexclude=['case3']\n");
    tree.put(
        "sets/last.toml",
        "[[step]]\nkind='exec'\nid='last'\nroot='a'\nprogram='/bin/last'\n",
    );
    let cases = tree.resolve("plan.toml").unwrap();
    let ids: Vec<&str> = cases.iter().map(|case| case.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "ltp:b:syscalls:beta",
            "ltp:b:syscalls:case2",
            "ltp:b:syscalls:case1",
            "ltp:a:syscalls:beta",
            "ltp:a:syscalls:case2",
            "ltp:a:syscalls:case1",
            "last"
        ]
    );
    assert_eq!(cases[1].source.file, tree.path("sets/first.toml"));
    assert_eq!(cases[1].source.entry, 1);
    assert_eq!(
        cases[1].source.catalog.as_deref(),
        Some(Path::new("/catalog"))
    );
    assert_eq!(cases[1].source.line, Some(2));
}

/// CLI 文件列表只替换入口 files，根和 defaults 仍属于入口。
#[test]
fn cli_files_replace_without_recursive_loading() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "files=['not-present.toml']\n[defaults]\ntimeout_sec=17\n{}",
            tree.roots()
        ),
    );
    tree.put(
        "chosen.toml",
        "[[step]]\nkind='exec'\nid='chosen'\nroot='b'\nprogram='/bin/true'\n",
    );
    let config = config::load(&tree.path("plan.toml"), &[PathBuf::from("chosen.toml")]).unwrap();
    let cases = selection::resolve(&config, "loongarch64").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "chosen");
    assert_eq!(cases[0].root, "b");
    assert_eq!(cases[0].timeout_sec, 17);
    tree.put("chosen.toml", "files=['nested.toml']\n");
    let error = config::load(&tree.path("plan.toml"), &[PathBuf::from("chosen.toml")]).unwrap_err();
    assert!(error.contains("chosen.toml") && error.contains("files"));
}

/// 关闭或架构不适用的选择不读取清单，也不运行任何准备或 Hook。
#[test]
fn disabled_arch_and_list_have_no_side_effects() {
    let tree = Tree::new();
    let marker = tree.path("must-not-exist");
    tree.put(
        "plan.toml",
        &format!(
            "files=['steps.toml']\n[roots.a]\npath={:?}\nprepare=[['/bin/sh','-c','touch {}']]\n",
            tree.path("root"),
            marker.display()
        ),
    );
    tree.put("steps.toml", &format!("[[step]]\nkind='ltp'\nenabled=false\nroots=['a']\nsuite='off'\ncatalog='/missing'\ninclude=['*']\n[[step]]\nkind='ltp'\narch=['loongarch64']\nroots=['a']\nsuite='la'\ncatalog='/also-missing'\ninclude=['*']\n[[step]]\nkind='exec'\nid='active'\nroot='a'\nprogram='/bin/true'\nhooks=true\nsetup=[['/bin/sh','-c','touch {}']]\ncleanup=[['/bin/sh','-c','touch {}']]\n", marker.display(), marker.display()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert!(cases[0].hooks);
    assert!(!marker.exists());
}

/// exec 与 Shell 交错不改变 argv，LTP 原文也不被拆词重建。
#[test]
fn mixed_commands_preserve_empty_arguments_and_shell_text() {
    let tree = Tree::new();
    let command = "MODE='value with spaces' printf '%s\\n' '' '雪 山' | cat  ";
    tree.put("root/catalog", &format!("tag {command}\n"));
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='exec'\nid='first'\nroot='a'\nprogram='./probe'\nargs=['','a b','雪 山','$literal']\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='mixed'\ncatalog='/catalog'\nshell='/custom/sh'\ninclude=['tag']\n[[step]]\nkind='shell'\nid='last'\nroot='a'\nshell='/bin/sh'\ncommand='echo last | cat'\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "ltp:a:mixed:tag", "last"]
    );
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "./probe".into(),
            args: vec!["".into(), "a b".into(), "雪 山".into(), "$literal".into()]
        }
    );
    assert_eq!(
        cases[1].command,
        CommandSpec::Shell {
            shell: "/custom/sh".into(),
            command: command.into()
        }
    );
}

/// 原清单的环境赋值、引号、空参数和管道交给实际 Shell 执行。
#[test]
fn runtest_tail_runs_with_original_shell_semantics() {
    let tree = Tree::new();
    tree.put(
        "root/catalog",
        "tag VALUE='two words' /bin/sh -c 'printf \"<%s><%s>\" \"$VALUE\" \"$1\"' sh '' | cat\n",
    );
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='shell'\ncatalog='/catalog'\nshell='/bin/sh'\ninclude=['tag']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    let CommandSpec::Shell { shell, command } = &cases[0].command else {
        panic!("not shell");
    };
    let output = Command::new(shell).args(["-c", command]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"<two words><>");
}

/// 各层环境按 key 合并，覆盖中的 argv、setup、cleanup 整字段替换。
#[test]
fn exact_override_merges_environment_and_replaces_arrays() {
    let tree = Tree::new();
    tree.put("plan.toml", &format!("files=['steps.toml']\noverride_files=['override.toml']\n[defaults]\nenv={{KEEP='default',LEVEL='default'}}\nsetup=[['/default']]\ncleanup=[['/default-clean']]\n[roots.a]\npath={:?}\nenv={{LEVEL='root',ROOT='yes'}}\n", tree.path("root")));
    tree.put("steps.toml", "[[step]]\nkind='exec'\nid='one'\nroot='a'\nprogram='/old'\nargs=['old','args']\nenv={LEVEL='step',STEP='yes'}\nsetup=[['/step']]\ncleanup=[['/step-clean']]\n");
    tree.put("override.toml", "[[override]]\nid='one'\nprogram='/new'\nargs=['','a b']\nenv={LEVEL='override',LOCAL='yes'}\nsetup=[]\ncleanup=[['/cleanup','x y']]\ntimeout_sec=19\nrepeat=3\nhooks=true\n");
    let cases = tree.resolve("plan.toml").unwrap();
    let case = &cases[0];
    assert_eq!(cases.len(), 1);
    assert_eq!(case.id, "one");
    assert_eq!(
        case.command,
        CommandSpec::Exec {
            program: "/new".into(),
            args: vec!["".into(), "a b".into()]
        }
    );
    assert_eq!(case.env["LEVEL"], "override");
    assert_eq!(case.env["KEEP"], "default");
    assert_eq!(case.env["ROOT"], "yes");
    assert_eq!(case.env["STEP"], "yes");
    assert_eq!(case.env["LOCAL"], "yes");
    assert!(case.setup.is_empty());
    assert_eq!(case.cleanup, vec![vec!["/cleanup", "x y"]]);
    assert_eq!(case.timeout_sec, 19);
    assert_eq!(case.repeat, 3);
    assert!(case.hooks);
}

/// 重复命中保留第一次来源；真正冲突必须由精确覆盖或新 ID 解决。
#[test]
fn duplicate_semantics_conflict_and_explicit_override() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!("files=['one.toml','two.toml']\n{}", tree.roots()),
    );
    let one = "[[step]]\nkind='exec'\nid='same'\nroot='a'\nprogram='/one'\n";
    tree.put("one.toml", one);
    tree.put("two.toml", one);
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].source.file, tree.path("one.toml"));
    tree.put("two.toml", &one.replace("/one", "/two"));
    let error = tree.resolve("plan.toml").unwrap_err();
    assert!(
        error.contains("conflicting selection ID")
            && error.contains("one.toml")
            && error.contains("two.toml")
    );
    tree.put(
        "plan.toml",
        &format!(
            "files=['one.toml','two.toml']\noverride_files=['fix.toml']\n{}",
            tree.roots()
        ),
    );
    tree.put("fix.toml", "[[override]]\nid='same'\nprogram='/fixed'\n");
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "/fixed".into(),
            args: vec![]
        }
    );
}

/// 覆盖不复活未选择项，冲突覆盖不采用最后一个值。
#[test]
fn overrides_reject_unselected_ids_and_field_conflicts() {
    let tree = Tree::new();
    tree.put("plan.toml", &format!("override_files=['override.toml']\n{}\n[[step]]\nkind='exec'\nid='one'\nroot='a'\nprogram='/bin/true'\n", tree.roots()));
    tree.put("override.toml", "[[override]]\nid='missing'\nrepeat=2\n");
    let error = tree.resolve("plan.toml").unwrap_err();
    assert!(error.contains("override.toml") && error.contains("not selected"));
    tree.put(
        "override.toml",
        "[[override]]\nid='one'\nrepeat=2\n[[override]]\nid='one'\nrepeat=3\n",
    );
    let error = tree.resolve("plan.toml").unwrap_err();
    assert!(error.contains("entry 2") && error.contains("entry 1") && error.contains("repeat"));
    tree.put(
        "override.toml",
        "[[override]]\nid='one'\nenv={A='one'}\n[[override]]\nid='one'\nenv={A='two'}\n",
    );
    assert!(tree
        .resolve("plan.toml")
        .unwrap_err()
        .contains("conflicting override env"));
    tree.put(
        "override.toml",
        "[[override]]\nid='one'\nrepeat=2\n[[override]]\nid='one'\ntimeout_sec=23\nrepeat=2\n",
    );
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!((cases[0].repeat, cases[0].timeout_sec), (2, 23));
}

/// 缺名、空模式与全排除分别保留具体来源、root 和原目录。
#[test]
fn selection_errors_report_the_actual_source() {
    let tree = Tree::new();
    tree.put("root/catalog", "known printf yes\n");
    tree.put(
        "plan.toml",
        &format!("files=['select.toml']\n{}", tree.roots()),
    );
    for (include, exclude, expected) in [
        ("missing", "", "name"),
        ("absent*", "", "pattern"),
        ("known", "known", "empty selection after exclude"),
    ] {
        tree.put("select.toml", &format!("[[step]]\nkind='ltp'\nroots=['a']\nsuite='s'\ncatalog='/catalog'\ninclude=[{include:?}]\nexclude=[{exclude:?}]\n").replace("exclude=[\"\"]", "exclude=[]"));
        let error = tree.resolve("plan.toml").unwrap_err();
        assert!(
            error.contains("select.toml:entry 1")
                && error.contains("root \"a\"")
                && error.contains("/catalog")
                && error.contains(expected),
            "{error}"
        );
    }
    tree.put(
        "select.toml",
        "[[step]]\nkind='exec'\nenabled=false\nid='off'\nroot='a'\nprogram='/missing'\n",
    );
    assert!(tree
        .resolve("plan.toml")
        .unwrap_err()
        .contains("empty selection"));
}

/// 所有 typed 输入层拒绝未知字段；被引用文件不能再次声明 include 树。
#[test]
fn unknown_fields_and_nested_files_are_rejected_at_input() {
    let tree = Tree::new();
    for text in [
        "typo=true",
        "[defaults]\ntimeuot_sec=2",
        "[roots.a]\npath='/'\nunknown=true",
        "[[step]]\nkind='exec'\nid='x'\nroot='a'\nprogram='/x'\nargz=[]",
    ] {
        tree.put("plan.toml", text);
        let error = config::load(&tree.path("plan.toml"), &[]).unwrap_err();
        assert!(
            error.contains("plan.toml") && error.contains("unknown field"),
            "{error}"
        );
    }
    tree.put(
        "plan.toml",
        &format!("files=['nested.toml']\n{}", tree.roots()),
    );
    tree.put("nested.toml", "override_files=['deeper.toml']");
    assert!(config::load(&tree.path("plan.toml"), &[])
        .unwrap_err()
        .contains("nested.toml"));
    tree.put(
        "plan.toml",
        &format!("override_files=['bad.toml']\n{}", tree.roots()),
    );
    tree.put("bad.toml", "[[override]]\nid='x'\nunknown=true");
    assert!(config::load(&tree.path("plan.toml"), &[])
        .unwrap_err()
        .contains("unknown field"));
}

/// 根名、路径、期限和命令形状的错误在输入处失败，不等到执行时吞掉。
#[test]
fn invalid_fields_and_missing_required_values_are_rejected() {
    let tree = Tree::new();
    let base = tree.roots();
    for tail in [
        "[[step]]\nkind='exec'\nroot='a'\nprogram='/x'",
        "[[step]]\nkind='exec'\nid='x'\nroot='unknown'\nprogram='/x'",
        "[[step]]\nkind='exec'\nid='x'\nroot='a'\nprogram='/x'\nrepeat=0",
        "[[step]]\nkind='exec'\nid='x'\nroot='a'\nprogram='/x'\ntimeout_sec=0",
        "[[step]]\nkind='exec'\nid='x'\nroot='a'\nprogram='/x'\ncwd='/../escape'",
        "[[step]]\nkind='exec'\nid='x'\nroot='a'\nprogram='/x'\ncommand='wrong kind'",
        "[[step]]\nkind='ltp'\nroots=['a']\nsuite='s'\ninclude=['x']",
        "[[step]]\nkind='basic'\nroots=['a']\ninclude=['[bad']",
        "[[step]]\nkind='libctest'\nroots=['a']\nvariant='guessed'\ninclude=['x']",
    ] {
        tree.put("plan.toml", &format!("{base}\n{tail}\n"));
        let error = config::load(&tree.path("plan.toml"), &[]).unwrap_err();
        assert!(error.contains("plan.toml:entry 1"), "{error}");
    }
}

/// 目录来源只选择真实可执行项并排序，basic 命令使用实际 basic 目录。
#[test]
fn executable_source_and_basic_use_actual_root_directories() {
    let tree = Tree::new();
    tree.executable("root/cases/zeta");
    tree.executable("root/cases/alpha");
    tree.put("root/cases/data", "not executable");
    fs::create_dir_all(tree.path("root/cases/directory")).unwrap();
    tree.executable("root/basic/mmap");
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nsource='executables'\nroots=['a']\nsuite='elf'\ndirectory='/cases'\ninclude=['*']\n[[step]]\nkind='basic'\nroots=['a']\ndirectory='/basic'\ninclude=['mmap']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<Vec<_>>(),
        ["ltp:a:elf:alpha", "ltp:a:elf:zeta", "basic:a:mmap"]
    );
    assert_eq!(cases[2].cwd, Path::new("/basic"));
    assert_eq!(
        cases[2].command,
        CommandSpec::Exec {
            program: "/basic/mmap".into(),
            args: vec![]
        }
    );
    assert_eq!(
        cases[2].protocol,
        Protocol::Basic {
            name: "mmap".into()
        }
    );
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='missing'\ncatalog='/runtest-missing'\ninclude=['*']\n", tree.roots()));
    let error = tree.resolve("plan.toml").unwrap_err();
    assert!(error.contains("runtest-missing") && error.contains("No such file"));
}

/// libctest 的可选名称来自原脚本，不生成缺失项，也不猜测聚合程序 selector。
#[test]
fn fixed_libctest_preserves_variant_selector_and_original_argv() {
    let tree = Tree::new();
    tree.put("root/run-dynamic.sh", "#!/bin/sh\n./runtest.exe -w entry-dynamic.exe env\n./runtest.exe -w entry-dynamic.exe argv\n");
    tree.put(
        "root/run-static.sh",
        "./runtest.exe -w entry-static.exe argv\n",
    );
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='libctest'\nroots=['b']\nvariant='dynamic'\ncatalog='/run-dynamic.sh'\ninclude=['*']\n[[step]]\nkind='libctest'\nroots=['a']\nvariant='static'\ncatalog='/run-static.sh'\ninclude=['argv']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<Vec<_>>(),
        [
            "libctest:b:dynamic:env",
            "libctest:b:dynamic:argv",
            "libctest:a:static:argv"
        ]
    );
    assert_eq!(
        cases[1].command,
        CommandSpec::Exec {
            program: "./runtest.exe".into(),
            args: vec!["-w".into(), "entry-dynamic.exe".into(), "argv".into()]
        }
    );
    assert_eq!(
        cases[1].protocol,
        Protocol::Pre2025Libctest {
            variant: "dynamic".into(),
            selector: "argv".into()
        }
    );
    assert_eq!(cases[1].source.line, Some(3));
    tree.put(
        "root/run-dynamic.sh",
        "./runtest.exe -w entry-static.exe argv\n",
    );
    assert!(tree
        .resolve("plan.toml")
        .unwrap_err()
        .contains("run-dynamic.sh:1"));
}

/// 千项实际清单有序展开，精确选一项后只执行该原命令并留下唯一标记。
#[test]
fn thousand_entry_catalog_and_single_selected_execution() {
    let tree = Tree::new();
    let mut catalog = String::new();
    for index in 0..1000 {
        writeln!(
            catalog,
            "case{index:04} printf 'case{index:04}'; : > marker-{index:04}"
        )
        .unwrap();
    }
    tree.put("root/catalog", &catalog);
    let plan = |include: &str| {
        format!("{}\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='large'\ncatalog='/catalog'\nshell='/bin/sh'\ninclude=[{include:?}]\n", tree.roots())
    };
    tree.put("plan.toml", &plan("case[0-9]*"));
    let all = tree.resolve("plan.toml").unwrap();
    assert_eq!(all.len(), 1000);
    assert_eq!(all[0].id, "ltp:a:large:case0000");
    assert_eq!(all[999].id, "ltp:a:large:case0999");
    assert!(!tree.path("root/marker-0000").exists());
    tree.put("plan.toml", &plan("case0591"));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    let CommandSpec::Shell { shell, command } = &cases[0].command else {
        panic!("not shell");
    };
    let output = Command::new(shell)
        .args(["-c", command])
        .current_dir(tree.path("root"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"case0591");
    let markers: Vec<_> = fs::read_dir(tree.path("root"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_str().unwrap().starts_with("marker-"))
        .collect();
    assert_eq!(markers, vec![std::ffi::OsString::from("marker-0591")]);
}

/// root 覆盖只修正已选 ID 的运行视图，环境和缺省 cwd 来自明确的新 root。
#[test]
fn root_override_resolves_duplicate_root_semantics_without_reselection() {
    let tree = Tree::new();
    tree.put("plan.toml", &format!("files=['one.toml','two.toml']\noverride_files=['override.toml']\n[roots.a]\npath={:?}\ncwd='/first'\nenv={{VIEW='a',OLD='only-a'}}\n[roots.b]\npath={:?}\ncwd='/second'\nenv={{VIEW='b'}}\n", tree.path("a"), tree.path("b")));
    let one = "[[step]]\nkind='exec'\nid='same'\nroot='a'\nprogram='/bin/true'\n";
    tree.put("one.toml", one);
    tree.put("two.toml", &one.replace("root='a'", "root='b'"));
    tree.put("override.toml", "[[override]]\nid='same'\nroot='b'\n");
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "same");
    assert_eq!(cases[0].root, "b");
    assert_eq!(cases[0].cwd, Path::new("/second"));
    assert_eq!(cases[0].env["VIEW"], "b");
    assert!(!cases[0].env.contains_key("OLD"));
    tree.put(
        "override.toml",
        "[[override]]\nid='same'\nroot='undeclared'\n",
    );
    assert!(tree
        .resolve("plan.toml")
        .unwrap_err()
        .contains("unknown root"));
}

/// directory 与 catalog 分属不同来源，禁用条目也不能带矛盾的配置字段。
#[test]
fn directory_and_catalog_are_not_implicit_aliases() {
    let tree = Tree::new();
    for fields in [
        "source='executables'\ncatalog='/cases'",
        "source='executables'\ndirectory='/cases'\ncatalog='/catalog'",
        "source='runtest'\ndirectory='/cases'",
        "source='runtest'\ncatalog='/catalog'\ndirectory='/cases'",
    ] {
        tree.put("plan.toml", &format!("{}\n[[step]]\nenabled=false\nkind='ltp'\nroots=['a']\nsuite='s'\ninclude=['*']\n{fields}\n", tree.roots()));
        assert!(config::load(&tree.path("plan.toml"), &[])
            .unwrap_err()
            .contains("entry 1"));
    }
    tree.put("plan.toml", &format!("{}\n[[step]]\nenabled=false\nkind='ltp'\nsource='executables'\nroots=['a']\nsuite='s'\ndirectory='/missing'\ninclude=['*']\n[[step]]\nkind='exec'\nid='active'\nroot='a'\nprogram='/bin/true'\n", tree.roots()));
    assert_eq!(tree.resolve("plan.toml").unwrap()[0].id, "active");
}

/// basic 的原始选择名不受自定义 root 名和后续 program 覆盖影响。
#[test]
fn basic_protocol_keeps_original_name_after_command_override() {
    let tree = Tree::new();
    tree.executable("root/basic/mkdir_");
    tree.put("plan.toml", &format!("override_files=['override.toml']\n[roots.unusual]\npath={:?}\n[[step]]\nkind='basic'\nroots=['unusual']\ndirectory='/basic'\ninclude=['mkdir_']\n", tree.path("root")));
    tree.put(
        "override.toml",
        "[[override]]\nid='basic:unusual:mkdir_'\nprogram='/other/program'\n",
    );
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        cases[0].protocol,
        Protocol::Basic {
            name: "mkdir_".into()
        }
    );
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "/other/program".into(),
            args: vec![]
        }
    );
}

/// 相对链接的可执行目标参与目录选择，稳定 ID 和执行路径保留原别名。
#[test]
fn executable_relative_symlink_preserves_alias_program_and_id() {
    let tree = Tree::new();
    tree.executable("root/bin/probe");
    fs::create_dir_all(tree.path("root/cases")).unwrap();
    symlink("../bin/probe", tree.path("root/cases/alias")).unwrap();
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nsource='executables'\nroots=['a']\nsuite='elf'\ndirectory='/cases'\ninclude=['alias']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "ltp:a:elf:alias");
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "/cases/alias".into(),
            args: vec![]
        }
    );
    assert_eq!(
        cases[0].source.catalog.as_deref(),
        Some(Path::new("/cases"))
    );
}

/// 清单的绝对链接从指定 root 解析，来源和命令仍保存原输入。
#[test]
fn absolute_catalog_symlink_reads_from_selected_root() {
    let tree = Tree::new();
    tree.put("root/sources/catalog", "probe printf 'root catalog'\n");
    symlink("/sources/catalog", tree.path("root/catalog")).unwrap();
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='linked'\ncatalog='/catalog'\ninclude=['probe']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "ltp:a:linked:probe");
    assert_eq!(
        cases[0].command,
        CommandSpec::Shell {
            shell: "/bin/sh".into(),
            command: "printf 'root catalog'".into()
        }
    );
    assert_eq!(
        cases[0].source.catalog.as_deref(),
        Some(Path::new("/catalog"))
    );
    assert_eq!(cases[0].source.line, Some(1));
}

/// 目录与清单来源的中间绝对链接都留在指定 root，执行路径不展开链接。
#[test]
fn intermediate_directory_symlink_sources_read_from_selected_root() {
    let tree = Tree::new();
    tree.executable("root/sources/cases/probe");
    tree.put("root/sources/catalog", "probe printf 'root catalog'\n");
    symlink("/sources", tree.path("root/linked")).unwrap();
    tree.put("plan.toml", &format!("{}\n[[step]]\nkind='ltp'\nsource='executables'\nroots=['a']\nsuite='elf'\ndirectory='/linked/cases'\ninclude=['probe']\n[[step]]\nkind='ltp'\nroots=['a']\nsuite='text'\ncatalog='/linked/catalog'\ninclude=['probe']\n", tree.roots()));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<Vec<_>>(),
        ["ltp:a:elf:probe", "ltp:a:text:probe"]
    );
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "/linked/cases/probe".into(),
            args: vec![]
        }
    );
    assert_eq!(
        cases[0].source.catalog.as_deref(),
        Some(Path::new("/linked/cases"))
    );
    assert_eq!(
        cases[1].command,
        CommandSpec::Shell {
            shell: "/bin/sh".into(),
            command: "printf 'root catalog'".into()
        }
    );
    assert_eq!(
        cases[1].source.catalog.as_deref(),
        Some(Path::new("/linked/catalog"))
    );
}

/// libctest 的原相对 runtest 命令继承显式 root cwd，不强制切回根目录。
#[test]
fn libctest_inherits_explicit_root_cwd_and_original_command() {
    let tree = Tree::new();
    tree.put(
        "root/libc-test/run-dynamic.sh",
        "./runtest.exe -w entry-dynamic.exe argv\n",
    );
    tree.put("plan.toml", &format!("[roots.a]\npath={:?}\ncwd='/libc-test'\n[[step]]\nkind='libctest'\nroots=['a']\nvariant='dynamic'\ncatalog='/libc-test/run-dynamic.sh'\ninclude=['argv']\n", tree.path("root")));
    let cases = tree.resolve("plan.toml").unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "libctest:a:dynamic:argv");
    assert_eq!(cases[0].cwd, Path::new("/libc-test"));
    assert_eq!(
        cases[0].command,
        CommandSpec::Exec {
            program: "./runtest.exe".into(),
            args: vec!["-w".into(), "entry-dynamic.exe".into(), "argv".into()]
        }
    );
    assert_eq!(
        cases[0].protocol,
        Protocol::Pre2025Libctest {
            variant: "dynamic".into(),
            selector: "argv".into()
        }
    );
}
