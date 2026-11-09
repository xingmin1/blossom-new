//! `contest-run` 计划加载（`config`）与用例选择展开（`selection`）的集成测试。
//!
//! 覆盖文件组合顺序、静态输入边界、root-major 展开、cwd/env/超时优先级、覆盖规则、`--only` 过滤、
//! 目录与清单来源的 root 内解析，以及仓库实际发布的计划文件。每个测试在自己的临时目录里构造计划与
//! 假测试根，互不共享可变状态，可并行运行。

use contest_run::config;
use contest_run::expand::expand;
use contest_run::model::{Config, ResolvedCase};
use contest_run::selection::{self, Selection};
use std::fmt::Write;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 为每个夹具分配唯一临时目录序号，避免并行测试互相覆盖。
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 测试专用的临时目录树，析构时整体删除。
struct Tree(PathBuf);

impl Tree {
    /// 在系统临时目录下建立带进程号与序号的空目录。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "contest-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// 把相对路径拼到夹具根下。
    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// 写入文本文件，自动创建缺失的父目录。
    fn put(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// 写入一个权限为 `0o755` 的最小可执行脚本，供目录扫描选中。
    fn executable(&self, relative: &str) {
        self.put(relative, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(self.path(relative), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 生成指向同一个 `root/` 的 `roots.a`、`roots.b` 配置片段。
    fn roots(&self) -> String {
        format!(
            "[roots.a]\npath = {:?}\n[roots.b]\npath = {:?}\n",
            self.path("root"),
            self.path("root")
        )
    }

    /// 加载夹具内的计划文件。
    fn load(&self, plan: &str) -> Result<Config, String> {
        config::load(&self.path(plan), &[])
    }

    /// 以 `riscv64` 加载并展开计划。
    fn resolve(&self, plan: &str) -> Result<Selection, String> {
        self.resolve_only(plan, &[])
    }

    /// 以 `riscv64` 加载计划并按 `only` 过滤展开。
    fn resolve_only(&self, plan: &str, only: &[String]) -> Result<Selection, String> {
        let config = self.load(plan)?;
        selection::resolve(&config, "riscv64", only)
    }

    /// 断言计划在选择阶段被拒绝并返回错误文本；成功说明测试前提已失效。
    fn error(&self, plan: &str) -> String {
        match self.resolve(plan) {
            Ok(_) => panic!("expected {plan} to be rejected"),
            Err(error) => error,
        }
    }

    /// 断言带 `only` 的选择被拒绝并返回错误文本；成功说明测试前提已失效。
    fn error_only(&self, plan: &str, only: &[String]) -> String {
        match self.resolve_only(plan, only) {
            Ok(_) => panic!("expected {plan} to be rejected for {only:?}"),
            Err(error) => error,
        }
    }
}

impl Drop for Tree {
    /// 测试结束时递归清理夹具目录。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 按执行顺序把全部结果组展平为用例序列。
fn cases(selection: &Selection) -> Vec<&ResolvedCase> {
    selection
        .groups
        .iter()
        .flat_map(|group| group.cases.iter())
        .collect()
}

/// 按执行顺序取出全部用例 ID，便于断言展开顺序。
fn ids(selection: &Selection) -> Vec<&str> {
    selection
        .groups
        .iter()
        .flat_map(|group| group.cases.iter())
        .map(|case| case.id.as_str())
        .collect()
}

/// 取首组首例，只在测试已确认唯一或只关心首例时使用。
fn first(selection: &Selection) -> &ResolvedCase {
    &selection.groups[0].cases[0]
}

/// 把字符串字面量列表转成 `selection::resolve` 的 `only` 参数。
fn only(patterns: &[&str]) -> Vec<String> {
    patterns
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .collect()
}

/// 单遍替换对未闭合花括号、空占位符与未知键必须原样保留，避免把普通文本当模板吞掉。
#[test]
fn expand_keeps_unknown_and_unterminated_braces() {
    assert_eq!(expand("tail {", &[("name", "x")]), "tail {");
    assert_eq!(expand("{} {missing}", &[("name", "x")]), "{} {missing}");
    assert_eq!(expand("{name} {name", &[("name", "x")]), "x {name");
}

/// 显式 ID 模板按 root 与名字替换，未写 ID 的步骤取 `{group}:{root}:{name}` 默认值。
#[test]
fn id_template_replaces_group_root_and_name() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"libctest\"\nroots = [\"a\"]\n\
             [[group.step]]\nid = \"libctest:{{root}}:dynamic:{{name}}\"\nnames = [\"alpha\"]\nexec = [\"/run\", \"{{name}}\"]\n\
             [[group.step]]\nid = \"libctest:{{root}}:static:{{name}}\"\nnames = [\"alpha\"]\nexec = [\"/run\", \"{{name}}\"]\n\
             [[group]]\nname = \"plain\"\nroots = [\"a\"]\n\
             [[group.step]]\nnames = [\"beta\"]\nexec = [\"/run\", \"{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        ids(&selection),
        [
            "libctest:a:dynamic:alpha",
            "libctest:a:static:alpha",
            "plain:a:beta"
        ]
    );
}

/// 空白 ID 无法写进设备消息，必须报错；504 字节是单条消息允许的最大 ID，505 字节即越界。
#[test]
fn case_id_with_whitespace_or_overlong_is_rejected() {
    let tree = Tree::new();
    let plan = |id: &str| {
        format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nid = {id:?}\nnames = [\"n\"]\nexec = [\"/x\"]\n",
            tree.roots()
        )
    };

    tree.put("plan.toml", &plan("bad id"));
    let error = tree.error("plan.toml");
    assert!(
        error.contains("invalid case ID"),
        "unexpected error: {error}"
    );

    let fits = "a".repeat(504);
    tree.put("plan.toml", &plan(&fits));
    assert_eq!(first(&tree.resolve("plan.toml").unwrap()).id, fits);

    let too_long = "a".repeat(505);
    tree.put("plan.toml", &plan(&too_long));
    let error = tree.error("plan.toml");
    assert!(error.contains("too long"), "unexpected error: {error}");
}

/// 全局 ID 重复必须报错并指出两处来源，覆盖跨文件两个分组撞 ID、同一组内两个步骤撞 ID，以及覆盖规则无法消解的情形。
#[test]
fn duplicate_case_ids_report_both_sources() {
    let tree = Tree::new();
    tree.put(
        "one.toml",
        "[[group]]\nname = \"one\"\nroots = [\"a\"]\n[[group.step]]\nid = \"same\"\nnames = [\"x\"]\nexec = [\"/x\"]\n",
    );
    tree.put(
        "two.toml",
        "[[group]]\nname = \"two\"\nroots = [\"a\"]\n[[group.step]]\nid = \"same\"\nnames = [\"x\"]\nexec = [\"/x\"]\n",
    );
    tree.put(
        "plan.toml",
        &format!("files = [\"one.toml\", \"two.toml\"]\n{}", tree.roots()),
    );
    let error = tree.error("plan.toml");
    assert!(
        error.contains("duplicate case ID")
            && error.contains("one.toml group 1 step 1")
            && error.contains("two.toml group 1 step 1"),
        "unexpected error: {error}"
    );

    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"fix.toml\"]\nfiles = [\"one.toml\", \"two.toml\"]\n{}",
            tree.roots()
        ),
    );
    tree.put(
        "fix.toml",
        "[[override]]\nid = \"same\"\nexec = [\"/fixed\"]\n",
    );
    let error = tree.error("plan.toml");
    assert!(
        error.contains("duplicate case ID"),
        "unexpected error: {error}"
    );

    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n\
             [[group.step]]\nid = \"same\"\nnames = [\"x\"]\nexec = [\"/x\"]\n\
             [[group.step]]\nid = \"same\"\nnames = [\"y\"]\nexec = [\"/y\"]\n",
            tree.roots()
        ),
    );
    let error = tree.error("plan.toml");
    assert!(
        error.contains("group 1 step 1") && error.contains("group 1 step 2"),
        "unexpected error: {error}"
    );
}

/// 一个分组在多个 root 上展开时按 root-major 排序：每个 root 先跑完全部步骤，且每个 (分组, root) 只产生一个结果组。
#[test]
fn root_major_order_emits_one_group_per_root() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"order\"\nroots = [\"a\", \"b\"]\n\
             start = \"START {{group}}-{{root}}\\n\"\nend = \"END {{group}}-{{root}}\\n\"\n\
             [[group.step]]\nid = \"dynamic:{{root}}:{{name}}\"\nnames = [\"one\", \"two\"]\nexec = [\"/d\"]\n\
             [[group.step]]\nid = \"static:{{root}}:{{name}}\"\nnames = [\"one\"]\nexec = [\"/s\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(selection.groups.len(), 2);
    assert_eq!(selection.groups[0].root, "a");
    assert_eq!(selection.groups[0].start, "START order-a\n");
    assert_eq!(selection.groups[0].end, "END order-a\n");
    assert_eq!(selection.groups[1].root, "b");
    assert_eq!(selection.groups[1].start, "START order-b\n");
    assert_eq!(
        ids(&selection),
        [
            "dynamic:a:one",
            "dynamic:a:two",
            "static:a:one",
            "dynamic:b:one",
            "dynamic:b:two",
            "static:b:one"
        ]
    );
}

/// 有效 cwd 依次取步骤、分组、测试根，相对目录按该 cwd 解析，绝对目录始终相对测试 root。
#[test]
fn cwd_precedence_drives_directory_resolution() {
    let tree = Tree::new();
    tree.executable("root/groupcwd/cases/fromgroup");
    tree.executable("root/stepcwd/cases/fromstep");
    tree.executable("root/rootcwd/cases/fromroot");
    tree.executable("root/abs/fromabs");
    tree.put(
        "plan.toml",
        &format!(
            "[roots.a]\npath = {:?}\ncwd = \"/rootcwd\"\n\
             [[group]]\nname = \"grouped\"\nroots = [\"a\"]\ncwd = \"/groupcwd\"\n\
             [[group.step]]\ndirectory = \"cases\"\nexec = [\"/cases/{{name}}\"]\n\
             [[group.step]]\ncwd = \"/stepcwd\"\ndirectory = \"cases\"\nexec = [\"/cases/{{name}}\"]\n\
             [[group.step]]\ncwd = \"/stepcwd\"\ndirectory = \"/abs\"\nexec = [\"/abs/{{name}}\"]\n\
             [[group]]\nname = \"rooted\"\nroots = [\"a\"]\n\
             [[group.step]]\ndirectory = \"cases\"\nexec = [\"/cases/{{name}}\"]\n",
            tree.path("root")
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let names: Vec<(&str, &Path)> = cases(&selection)
        .iter()
        .map(|case| (case.name.as_str(), case.cwd.as_path()))
        .collect();
    assert_eq!(
        names,
        [
            ("fromgroup", Path::new("/groupcwd")),
            ("fromstep", Path::new("/stepcwd")),
            ("fromabs", Path::new("/stepcwd")),
            ("fromroot", Path::new("/rootcwd"))
        ]
    );
}

/// 环境变量按 root、分组、步骤逐层按键覆盖；步骤的超时与重复次数覆盖 defaults，未指定时仍取 defaults。
#[test]
fn environment_and_timing_follow_layer_priority() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "[defaults]\ntimeout_sec = 17\nrepeat = 2\n\
             [roots.a]\npath = {:?}\nenv = {{ R = \"root\", SAME = \"root\", ROOT = \"yes\" }}\n\
             [[group]]\nname = \"g\"\nroots = [\"a\"]\nenv = {{ G = \"group\", SAME = \"group\" }}\n\
             [[group.step]]\nnames = [\"full\"]\nexec = [\"/x\"]\nenv = {{ S = \"step\", SAME = \"step\" }}\ntimeout_sec = 5\nrepeat = 3\n\
             [[group.step]]\nnames = [\"plain\"]\nexec = [\"/x\"]\n",
            tree.path("root")
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let full = &selection.groups[0].cases[0];
    assert_eq!(full.env["SAME"], "step");
    assert_eq!(full.env["R"], "root");
    assert_eq!(full.env["G"], "group");
    assert_eq!(full.env["S"], "step");
    assert_eq!(full.env["ROOT"], "yes");
    assert_eq!(full.timeout_sec, 5);
    assert_eq!(full.repeat, 3);
    let plain = &selection.groups[0].cases[1];
    assert_eq!(plain.env["SAME"], "group");
    assert!(!plain.env.contains_key("S"));
    assert_eq!(plain.timeout_sec, 17);
    assert_eq!(plain.repeat, 2);
}

/// `--only` 只按完整 ID 缩小已选集合：不加入新用例、丢掉空组，过滤后为空或模式非法都报错。
#[test]
fn only_narrows_full_ids_and_rejects_empty_results() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"one\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"first\", \"second\"]\nexec = [\"/x\"]\n\
             [[group]]\nname = \"two\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"third\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );

    let selection = tree
        .resolve_only("plan.toml", &only(&["one:a:first"]))
        .unwrap();
    assert_eq!(selection.groups.len(), 1);
    assert_eq!(ids(&selection), ["one:a:first"]);

    let selection = tree
        .resolve_only("plan.toml", &only(&["one:a:sec*"]))
        .unwrap();
    assert_eq!(ids(&selection), ["one:a:second"]);

    let selection = tree.resolve_only("plan.toml", &only(&["one:*"])).unwrap();
    assert_eq!(ids(&selection), ["one:a:first", "one:a:second"]);

    for pattern in ["first", "two:a:missing"] {
        let error = tree.error_only("plan.toml", &only(&[pattern]));
        assert!(
            error.contains("empty selection"),
            "unexpected error: {error}"
        );
    }
    let error = tree.error_only("plan.toml", &only(&["["]));
    assert!(
        error.contains("invalid pattern"),
        "unexpected error: {error}"
    );
}

/// 目录来源只取普通且有执行位的文件并排序；include 省略等于全选，重叠模式去重，exclude 无命中不报错。
#[test]
fn directory_scan_sorts_executables_and_include_dedups() {
    let tree = Tree::new();
    tree.executable("root/cases/zeta");
    tree.executable("root/cases/alpha");
    tree.put("root/cases/data", "not executable");
    fs::create_dir_all(tree.path("root/cases/nested")).unwrap();
    tree.executable("root/cases/nested/deep");

    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"scan\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/cases\"\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["scan:a:alpha", "scan:a:zeta"]);
    assert_eq!(first(&selection).argv, ["/cases/alpha"]);
    assert_eq!(
        first(&selection).source.catalog.as_deref(),
        Some(Path::new("/cases"))
    );
    assert_eq!(first(&selection).source.line, None);

    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"scan\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/cases\"\ninclude = [\"alpha*\", \"alpha\"]\nexclude = [\"missing\"]\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["scan:a:alpha"]);
}

/// 任一 include 模式在候选里完全没有命中都必须报错并指明步骤与来源路径，避免静默少跑用例。
#[test]
fn include_without_match_reports_step_and_path() {
    let tree = Tree::new();
    tree.executable("root/cases/alpha");
    let plan = |include: &str| {
        format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/cases\"\ninclude = [{include}]\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        )
    };

    tree.put("plan.toml", &plan("\"alpha\""));
    assert_eq!(ids(&tree.resolve("plan.toml").unwrap()), ["g:a:alpha"]);

    tree.put("plan.toml", &plan("\"alpha\", \"zzz\""));
    let error = tree.error("plan.toml");
    assert!(
        error.contains("group 1 step 1")
            && error.contains("/cases")
            && error.contains("matched no candidate"),
        "unexpected error: {error}"
    );
}

/// names 的元素就是名字本身，不展开 glob；与 include/exclude 同时出现属于双重选择，加载即拒绝。
#[test]
fn names_are_literal_and_reject_pattern_fields() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"open*\", \"plain\"]\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["g:a:open*", "g:a:plain"]);
    assert_eq!(first(&selection).name, "open*");

    for extra in ["include = [\"*\"]", "exclude = [\"*\"]"] {
        tree.put(
            "plan.toml",
            &format!(
                "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"plain\"]\n{extra}\nexec = [\"/cases/{{name}}\"]\n",
                tree.roots()
            ),
        );
        let error = tree.load("plan.toml").unwrap_err();
        assert!(
            error.contains("group 1 step 1") && error.contains("explicit selection"),
            "unexpected error: {error}"
        );
    }
}

/// 覆盖规则整体替换 argv、setup、cleanup 与模板，env 按键合并；未覆盖字段仍取步骤值与 defaults。
#[test]
fn override_replaces_arrays_and_merges_environment() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"override.toml\"]\n\
             [defaults]\ntimeout_sec = 17\nrepeat = 2\n\
             [roots.a]\npath = {:?}\nenv = {{ R = \"root\", SAME = \"root\", ROOT = \"yes\" }}\n\
             [[group]]\nname = \"g\"\nroots = [\"a\"]\nenv = {{ G = \"group\", SAME = \"group\" }}\n\
             [[group.step]]\nnames = [\"case\"]\nexec = [\"/old\", \"{{name}}\"]\nenv = {{ S = \"step\", SAME = \"step\" }}\n\
             timeout_sec = 5\nrepeat = 3\nsetup = [[\"/old-setup\"]]\ncleanup = [[\"/old-clean\"]]\n\
             before = \"BEFORE {{name}}\\n\"\nafter = \"AFTER {{name}} {{status}}\\n\"\n\
             [[group.step]]\nnames = [\"plain\"]\nexec = [\"/bin/plain\"]\n",
            tree.path("root")
        ),
    );
    tree.put(
        "override.toml",
        "[[override]]\nid = \"g:a:case\"\nexec = [\"/new\", \"{name}\", \"a b\"]\n\
         env = { O = \"override\", SAME = \"override\" }\ntimeout_sec = 19\nrepeat = 4\n\
         setup = []\ncleanup = [[\"/cleanup\", \"x y\"]]\nbefore = \"OVERRIDE {id}\\n\"\nafter = \"TAIL {status}\\n\"\n\
         [[override]]\nid = \"g:a:case\"\nenv = { SAME = \"later\" }\ntimeout_sec = 23\n",
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let case = &selection.groups[0].cases[0];
    assert_eq!(case.id, "g:a:case");
    assert_eq!(case.group, "g");
    assert_eq!(case.root, "a");
    assert_eq!(case.name, "case");
    assert_eq!(case.argv, ["/new", "case", "a b"]);
    assert_eq!(case.env["SAME"], "later");
    assert_eq!(case.env["R"], "root");
    assert_eq!(case.env["G"], "group");
    assert_eq!(case.env["S"], "step");
    assert_eq!(case.env["O"], "override");
    assert!(case.setup.is_empty());
    assert_eq!(
        case.cleanup,
        vec![vec!["/cleanup".to_owned(), "x y".to_owned()]]
    );
    assert_eq!(case.before, "OVERRIDE g:a:case\n");
    assert_eq!(case.after, "TAIL {status}\n");
    assert_eq!(case.timeout_sec, 23);
    assert_eq!(case.repeat, 4);

    let plain = &selection.groups[0].cases[1];
    assert_eq!(plain.timeout_sec, 17);
    assert_eq!(plain.repeat, 2);
    assert!(!plain.env.contains_key("O"));
}

/// 覆盖规则指向未被选中的 ID 只记入 unmatched 且不阻止运行，命中的规则照常应用。
#[test]
fn unmatched_override_is_reported_without_blocking() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"override.toml\"]\n{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"known\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    tree.put(
        "override.toml",
        "[[override]]\nid = \"absent\"\ntimeout_sec = 5\n[[override]]\nid = \"g:a:known\"\ntimeout_sec = 9\n",
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        selection.unmatched,
        [format!(
            "absent at {} override 1",
            tree.path("override.toml").display()
        )]
    );
    assert_eq!(first(&selection).timeout_sec, 9);
}

/// 架构不匹配的覆盖规则既不生效也不进入 unmatched；架构匹配时才应用。
#[test]
fn override_arch_filter_skips_rule_entirely() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"override.toml\"]\n{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"known\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    tree.put(
        "override.toml",
        "[[override]]\nid = \"g:a:known\"\narch = [\"loongarch64\"]\nrepeat = 9\n",
    );
    let config = tree.load("plan.toml").unwrap();
    let selection = selection::resolve(&config, "riscv64", &[]).unwrap();
    assert_eq!(first(&selection).repeat, 1);
    assert!(selection.unmatched.is_empty());
    let selection = selection::resolve(&config, "loongarch64", &[]).unwrap();
    assert_eq!(first(&selection).repeat, 9);
    assert!(selection.unmatched.is_empty());
}

/// 覆盖规则不能改变用例归属与生命周期开关，出现此类字段时加载即拒绝并指出文件。
#[test]
fn override_rejects_unchangeable_fields() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"override.toml\"]\n{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"known\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    for fields in ["root = \"/elsewhere\"", "hooks = true"] {
        tree.put(
            "override.toml",
            &format!("[[override]]\nid = \"g:a:known\"\n{fields}\n"),
        );
        let error = tree.load("plan.toml").unwrap_err();
        assert!(
            error.contains("override.toml") && error.contains("unknown field"),
            "unexpected error: {error}"
        );
    }
}

/// 覆盖规则在完整选中集合上应用后才执行 `--only`；被过滤掉的用例命中覆盖时不得误报 unmatched。
#[test]
fn override_applies_to_full_selection_before_only_filter() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "override_files = [\"override.toml\"]\n{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"alpha\", \"beta\"]\nexec = [\"/old\"]\n",
            tree.roots()
        ),
    );
    tree.put(
        "override.toml",
        "[[override]]\nid = \"g:a:alpha\"\nexec = [\"/patched\", \"{name}\"]\n[[override]]\nid = \"g:a:beta\"\ntimeout_sec = 9\n",
    );
    let selection = tree
        .resolve_only("plan.toml", &only(&["g:a:alpha"]))
        .unwrap();
    assert_eq!(ids(&selection), ["g:a:alpha"]);
    assert_eq!(first(&selection).argv, ["/patched", "alpha"]);
    assert!(selection.unmatched.is_empty());
}

/// 同一 (组名, root) 被两个分组声明会让同名评分组被拆成两次；加载阶段必须拒绝并指出两处来源。
#[test]
fn duplicate_group_root_is_rejected_at_load() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\n\
             [[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"y\"]\nexec = [\"/y\"]\n",
            tree.roots()
        ),
    );
    let error = tree.load("plan.toml").unwrap_err();
    assert!(
        error.contains("duplicate group") && error.contains("group 1") && error.contains("group 2"),
        "unexpected error: {error}"
    );

    let contest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../contest");
    tree.put(
        "plan.toml",
        &format!(
            "files = [{:?}, {:?}]\n[roots.musl]\npath = \"/\"\n[roots.glibc]\npath = \"/\"\n",
            contest.join("sets/ltp.toml"),
            contest.join("sets/ltp-catalog.toml")
        ),
    );
    let error = tree.load("plan.toml").unwrap_err();
    assert!(
        error.contains("duplicate group")
            && error.contains("ltp.toml")
            && error.contains("ltp-catalog.toml"),
        "unexpected error: {error}"
    );
}

/// `..` 在测试根处被截断，相对目录不能借父组件逃逸到宿主文件系统。
#[test]
fn parent_components_stop_at_root() {
    let tree = Tree::new();
    tree.executable("outside/escaped");
    tree.executable("root/outside/inner");
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"../outside\"\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["g:a:inner"]);
}

/// 符号链接自环必须在受限路径解析阶段报 ELOOP，而不是无限重试或读错目录。
#[test]
fn symlink_loop_reports_eloop() {
    let tree = Tree::new();
    fs::create_dir_all(tree.path("root")).unwrap();
    symlink("loop", tree.path("root/loop")).unwrap();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/loop\"\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    let error = tree.error("plan.toml");
    let eloop = std::io::Error::from_raw_os_error(libc::ELOOP).to_string();
    assert!(
        error.contains("/loop") && error.contains(&eloop),
        "unexpected error: {error}"
    );
}

/// 清单命令只去掉行首空白，尾部空格与其他字符原样进入 `{command}` 且不再展开；after 保留原始模板。
#[test]
fn catalog_command_is_verbatim_and_after_stays_a_template() {
    let tree = Tree::new();
    tree.put("root/catalog", "fork01 FOO=1 {name} > /tmp/out  \n");
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"ltp\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = \"/catalog\"\ninclude = [\"fork01\"]\n\
             exec = [\"/busybox\", \"sh\", \"-c\", \"{{command}}\"]\nbefore = \"RUN {{name}}\\n\"\nafter = \"FAIL {{name}} : {{status}}\\n\"\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let case = first(&selection);
    assert_eq!(
        case.catalog_command.as_deref(),
        Some("FOO=1 {name} > /tmp/out  ")
    );
    assert_eq!(
        case.argv,
        ["/busybox", "sh", "-c", "FOO=1 {name} > /tmp/out  "]
    );
    assert_eq!(case.before, "RUN fork01\n");
    assert_eq!(case.after, "FAIL {name} : {status}\n");
    assert_eq!(
        expand(&case.after, &[("name", "fork01"), ("status", "7")]),
        "FAIL fork01 : 7\n"
    );
    assert_eq!(case.source.line, Some(1));
}

/// 清单行缺少命令或命令为空时报错，并带出清单路径与行号，避免坏行被静默跳过。
#[test]
fn catalog_line_errors_report_line_numbers() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = \"/catalog\"\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );

    tree.put("root/catalog", "# comment\n\nlonely\n");
    let error = tree.error("plan.toml");
    assert!(
        error.contains("/catalog:3") && error.contains("line has no command"),
        "unexpected error: {error}"
    );

    tree.put("root/catalog", "ok printf ok\ntag   \n");
    let error = tree.error("plan.toml");
    assert!(
        error.contains("/catalog:2") && error.contains("empty command"),
        "unexpected error: {error}"
    );
}

/// 清单缺失或指向目录时直接报错并带路径，不能悄悄回退成目录扫描。
#[test]
fn missing_catalog_does_not_fall_back_to_directory() {
    let tree = Tree::new();
    let plan = |catalog: &str| {
        format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = {catalog:?}\nexec = [\"/x\"]\n",
            tree.roots()
        )
    };

    tree.put("plan.toml", &plan("/missing"));
    let error = tree.error("plan.toml");
    let enoent = std::io::Error::from_raw_os_error(libc::ENOENT).to_string();
    assert!(
        error.contains("/missing") && error.contains(&enoent),
        "unexpected error: {error}"
    );

    tree.executable("root/list/child");
    tree.put("plan.toml", &plan("/list"));
    let error = tree.error("plan.toml");
    let eisdir = std::io::Error::from_raw_os_error(libc::EISDIR).to_string();
    assert!(
        error.contains("/list") && error.contains(&eisdir),
        "unexpected error: {error}"
    );
}

/// 多文件按声明顺序拼接，组内按 root-major、include 顺序与清单行顺序展开，来源记录文件、条目、路径与行号。
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
            "files = [\"sets/first.toml\", \"sets/last.toml\"]\n{}",
            tree.roots()
        ),
    );
    tree.put(
        "sets/first.toml",
        "[[group]]\nname = \"syscalls\"\nroots = [\"b\", \"a\"]\n\
         [[group.step]]\ncatalog = \"/catalog\"\ninclude = [\"beta\", \"case[12]\", \"case?\"]\nexclude = [\"case3\"]\n\
         exec = [\"/bin/sh\", \"-c\", \"{command}\"]\n",
    );
    tree.put(
        "sets/last.toml",
        "[[group]]\nname = \"last\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"only\"]\nexec = [\"/bin/last\"]\n",
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(
        ids(&selection),
        [
            "syscalls:b:beta",
            "syscalls:b:case2",
            "syscalls:b:case1",
            "syscalls:a:beta",
            "syscalls:a:case2",
            "syscalls:a:case1",
            "last:a:only"
        ]
    );
    let case = &selection.groups[0].cases[1];
    assert_eq!(case.source.file, tree.path("sets/first.toml"));
    assert_eq!(case.source.entry, "group 1 step 1");
    assert_eq!(case.source.catalog.as_deref(), Some(Path::new("/catalog")));
    assert_eq!(case.source.line, Some(2));
    assert_eq!(case.argv, ["/bin/sh", "-c", "printf two"]);
}

/// 命令行文件列表整体替换主计划的 `files` 且保留 roots/defaults；引用的分组文件不允许再嵌套文件列表。
#[test]
fn cli_files_replace_without_recursive_loading() {
    let tree = Tree::new();
    tree.put(
        "plan.toml",
        &format!(
            "files = [\"not-present.toml\"]\n[defaults]\ntimeout_sec = 17\n{}",
            tree.roots()
        ),
    );
    tree.put(
        "chosen.toml",
        "[[group]]\nname = \"chosen\"\nroots = [\"b\"]\n[[group.step]]\nnames = [\"only\"]\nexec = [\"/bin/true\"]\n",
    );
    let loaded = config::load(&tree.path("plan.toml"), &[PathBuf::from("chosen.toml")]).unwrap();
    assert_eq!(loaded.defaults.timeout_sec, 17);
    let selection = selection::resolve(&loaded, "loongarch64", &[]).unwrap();
    let case = first(&selection);
    assert_eq!(case.id, "chosen:b:only");
    assert_eq!(case.root, "b");
    assert_eq!(case.timeout_sec, 17);

    tree.put("chosen.toml", "files = [\"nested.toml\"]\n");
    let error = config::load(&tree.path("plan.toml"), &[PathBuf::from("chosen.toml")]).unwrap_err();
    assert!(
        error.contains("chosen.toml") && error.contains("unknown field"),
        "unexpected error: {error}"
    );
}

/// 禁用或架构不匹配的步骤不读清单，加载与选择阶段也不执行 prepare/setup/cleanup。
#[test]
fn disabled_arch_and_selection_have_no_side_effects() {
    let tree = Tree::new();
    let marker = tree.path("must-not-exist");
    tree.put(
        "plan.toml",
        &format!(
            "files = [\"steps.toml\"]\n[roots.a]\npath = {:?}\nprepare = [[\"/bin/sh\", \"-c\", \"touch {}\"]]\n",
            tree.path("root"),
            marker.display()
        ),
    );
    tree.put(
        "steps.toml",
        &format!(
            "[[group]]\nname = \"g\"\nroots = [\"a\"]\n\
             [[group.step]]\nenabled = false\ncatalog = \"/missing\"\nexec = [\"/x\"]\n\
             [[group.step]]\narch = [\"loongarch64\"]\ncatalog = \"/also-missing\"\nexec = [\"/x\"]\n\
             [[group.step]]\nnames = [\"active\"]\nexec = [\"/bin/true\"]\n\
             setup = [[\"/bin/sh\", \"-c\", \"touch {}\"]]\ncleanup = [[\"/bin/sh\", \"-c\", \"touch {}\"]]\n",
            marker.display(),
            marker.display()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["g:a:active"]);
    assert!(!marker.exists());
}

/// argv 模板保留空串、空格、Unicode 与 `$` 字面量；清单命令原文（含尾部空格）整体进入 `{command}`。
#[test]
#[expect(
    clippy::literal_string_with_formatting_args,
    reason = "计划文本中的 {{command}} 经 format! 转义后是 expand 的占位符，lint 对转义花括号误报"
)]
fn mixed_commands_preserve_arguments_and_catalog_text() {
    let tree = Tree::new();
    let command = "VALUES='x y' printf '%s\\n' | cat  ";
    tree.put("root/catalog", &format!("tag {command}\n"));
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"mixed\"\nroots = [\"a\"]\n\
             [[group.step]]\nid = \"argv:{{name}}\"\nnames = [\"probe\"]\nexec = [\"./probe\", \"\", \"a b\", \"雪 山\", \"$literal\"]\n\
             [[group.step]]\ncatalog = \"/catalog\"\ninclude = [\"tag\"]\nexec = [\"/custom/sh\", \"-c\", \"{{command}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["argv:probe", "mixed:a:tag"]);
    assert_eq!(
        selection.groups[0].cases[0].argv,
        ["./probe", "", "a b", "雪 山", "$literal"]
    );
    let case = &selection.groups[0].cases[1];
    assert_eq!(case.catalog_command.as_deref(), Some(command));
    assert_eq!(case.argv, ["/custom/sh", "-c", command]);
}

/// 清单命令交给 `sh -c` 时保持内联赋值、引号、空位置参数与管道的原始语义。
#[test]
fn catalog_command_keeps_shell_semantics() {
    let tree = Tree::new();
    tree.put(
        "root/catalog",
        "tag VALUE='two words' /bin/sh -c 'printf \"<%s><%s>\" \"$VALUE\" \"$1\"' sh '' | cat\n",
    );
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"shell\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = \"/catalog\"\ninclude = [\"tag\"]\nexec = [\"/bin/sh\", \"-c\", \"{{command}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let argv = &first(&selection).argv;
    let output = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"<two words><>");
}

/// 所有 TOML 层级都拒绝未知字段，已删除的旧字段与子文件嵌套声明在加载阶段即失败并指出文件。
#[test]
fn unknown_fields_and_nested_files_are_rejected_at_input() {
    let tree = Tree::new();
    let base = tree.roots();
    for extra in [
        "typo = true\n",
        "[defaults]\nhooks = true\n",
        "[roots.c]\npath = \"/\"\nkind = \"x\"\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\nkind = \"exec\"\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\nhooks = true\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\nprogram = \"/x\"\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\nargs = []\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\nsuite = \"s\"\n",
        "[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nnames = [\"x\"]\nexec = [\"/x\"]\nvariant = \"v\"\n",
    ] {
        tree.put("plan.toml", &format!("{extra}{base}"));
        let error = tree.load("plan.toml").unwrap_err();
        assert!(
            error.contains("plan.toml") && error.contains("unknown field"),
            "unexpected error for {extra:?}: {error}"
        );
    }

    tree.put("plan.toml", &format!("files = [\"nested.toml\"]\n{base}"));
    tree.put("nested.toml", "files = [\"deeper.toml\"]\n");
    let error = tree.load("plan.toml").unwrap_err();
    assert!(
        error.contains("nested.toml") && error.contains("unknown field"),
        "unexpected error: {error}"
    );

    tree.put(
        "plan.toml",
        &format!("override_files = [\"bad.toml\"]\n{base}"),
    );
    tree.put("bad.toml", "[[override]]\nid = \"x\"\nunknown = true\n");
    let error = tree.load("plan.toml").unwrap_err();
    assert!(
        error.contains("bad.toml") && error.contains("unknown field"),
        "unexpected error: {error}"
    );
}

/// 加载阶段拒绝非正的超时/重复次数、逃逸路径、非法 glob/架构、空 argv、错误的来源组合与缺失的组字段。
#[test]
fn invalid_fields_are_rejected_at_load() {
    let tree = Tree::new();
    let base = tree.roots();
    let step =
        |body: &str| format!("[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\n{body}\n");
    let rows = [
        (
            step("names = [\"n\"]\nexec = [\"/x\"]\nrepeat = 0"),
            "positive",
        ),
        (
            step("names = [\"n\"]\nexec = [\"/x\"]\ntimeout_sec = 0"),
            "positive",
        ),
        (
            step("names = [\"n\"]\nexec = [\"/x\"]\ncwd = \"/../escape\""),
            "expected absolute path",
        ),
        (
            step("directory = \"/\"\ninclude = [\"[bad\"]\nexec = [\"/x\"]"),
            "invalid pattern",
        ),
        (
            "[[group]]\nname = \"g\"\nroots = [\"missing\"]\n[[group.step]]\nnames = [\"n\"]\nexec = [\"/x\"]\n".to_owned(),
            "unknown root",
        ),
        (
            step("names = [\"n\"]\ndirectory = \"/\"\nexec = [\"/x\"]"),
            "exactly one",
        ),
        (step("exec = [\"/x\"]"), "exactly one"),
        (
            step("names = [\"n\"]\ninclude = [\"*\"]\nexec = [\"/x\"]"),
            "explicit selection",
        ),
        (step("names = []\nexec = [\"/x\"]"), "names must not be empty"),
        (
            step("names = [\"bad name\"]\nexec = [\"/x\"]"),
            "invalid identifier",
        ),
        (
            step("catalog = \"catalog\"\nexec = [\"/x\"]"),
            "expected absolute path",
        ),
        (
            step("arch = [\"x86_64\"]\nnames = [\"n\"]\nexec = [\"/x\"]"),
            "unknown architecture",
        ),
        (step("names = [\"n\"]\nexec = []"), "exec must not be empty"),
        (
            "[[group]]\nname = \"g\"\nroots = []\n[[group.step]]\nnames = [\"n\"]\nexec = [\"/x\"]\n".to_owned(),
            "roots must not be empty",
        ),
        (
            "[[group]]\nname = \"g\"\nroots = [\"a\"]\nstep = []\n".to_owned(),
            "step must not be empty",
        ),
        (
            "[[group]]\nname = \"g\"\nroots = [\"a\"]\ncwd = \"relative\"\n[[group.step]]\nnames = [\"n\"]\nexec = [\"/x\"]\n".to_owned(),
            "expected absolute path",
        ),
        (
            step("directory = \"/\"\ncatalog = \"/catalog\"\nexec = [\"/x\"]"),
            "exactly one",
        ),
    ];
    for (tail, expected) in rows {
        tree.put("plan.toml", &format!("{base}{tail}\n"));
        let error = tree.load("plan.toml").unwrap_err();
        assert!(
            error.contains("plan.toml") && error.contains(expected),
            "expected message {expected:?} in: {error}"
        );
    }
}

/// 目录扫描跟随相对符号链接检查执行位，名字与 argv 保留链接别名而不是真实目标。
#[test]
fn executable_relative_symlink_preserves_alias_name() {
    let tree = Tree::new();
    tree.executable("root/bin/probe");
    fs::create_dir_all(tree.path("root/cases")).unwrap();
    symlink("../bin/probe", tree.path("root/cases/alias")).unwrap();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"elf\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/cases\"\ninclude = [\"alias\"]\nexec = [\"/cases/{{name}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["elf:a:alias"]);
    assert_eq!(first(&selection).argv, ["/cases/alias"]);
    assert_eq!(
        first(&selection).source.catalog.as_deref(),
        Some(Path::new("/cases"))
    );
}

/// 绝对符号链接在目标根内重新定根，清单从所选 root 读取且来源保留配置路径。
#[test]
fn absolute_catalog_symlink_reads_from_selected_root() {
    let tree = Tree::new();
    tree.put("root/sources/catalog", "probe printf 'root catalog'\n");
    symlink("/sources/catalog", tree.path("root/catalog")).unwrap();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"linked\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = \"/catalog\"\ninclude = [\"probe\"]\nexec = [\"/bin/sh\", \"-c\", \"{{command}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    let case = first(&selection);
    assert_eq!(case.id, "linked:a:probe");
    assert_eq!(case.argv, ["/bin/sh", "-c", "printf 'root catalog'"]);
    assert_eq!(case.source.catalog.as_deref(), Some(Path::new("/catalog")));
    assert_eq!(case.source.line, Some(1));
}

/// 中间目录是绝对符号链接时，目录扫描与清单读取都限制在目标根内，路径保持配置原样。
#[test]
fn intermediate_directory_symlink_reads_from_selected_root() {
    let tree = Tree::new();
    tree.executable("root/sources/cases/probe");
    tree.put("root/sources/catalog", "probe printf 'root catalog'\n");
    symlink("/sources", tree.path("root/linked")).unwrap();
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"elf\"\nroots = [\"a\"]\n\
             [[group.step]]\nid = \"dir:{{root}}:{{name}}\"\ndirectory = \"/linked/cases\"\ninclude = [\"probe\"]\nexec = [\"/linked/cases/{{name}}\"]\n\
             [[group.step]]\nid = \"cat:{{root}}:{{name}}\"\ncatalog = \"/linked/catalog\"\ninclude = [\"probe\"]\nexec = [\"/bin/sh\", \"-c\", \"{{command}}\"]\n",
            tree.roots()
        ),
    );
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["dir:a:probe", "cat:a:probe"]);
    assert_eq!(selection.groups[0].cases[0].argv, ["/linked/cases/probe"]);
    assert_eq!(
        selection.groups[0].cases[0].source.catalog.as_deref(),
        Some(Path::new("/linked/cases"))
    );
    assert_eq!(
        selection.groups[0].cases[1].argv,
        ["/bin/sh", "-c", "printf 'root catalog'"]
    );
    assert_eq!(
        selection.groups[0].cases[1].source.catalog.as_deref(),
        Some(Path::new("/linked/catalog"))
    );
}

/// 千级清单保序展开，精确选择后只剩目标条目且 argv 取自清单原文。
#[test]
fn thousand_entry_catalog_preserves_order() {
    let tree = Tree::new();
    let mut catalog = String::new();
    for index in 0..1000 {
        writeln!(catalog, "case{index:04} printf 'case{index:04}'").unwrap();
    }
    tree.put("root/catalog", &catalog);
    let plan = |include: &str| {
        format!(
            "{}[[group]]\nname = \"large\"\nroots = [\"a\"]\n[[group.step]]\ncatalog = \"/catalog\"\ninclude = [{include}]\nexec = [\"/bin/sh\", \"-c\", \"{{command}}\"]\n",
            tree.roots()
        )
    };

    tree.put("plan.toml", &plan("\"case[0-9]*\""));
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(selection.groups[0].cases.len(), 1000);
    assert_eq!(selection.groups[0].cases[0].id, "large:a:case0000");
    assert_eq!(selection.groups[0].cases[999].id, "large:a:case0999");

    tree.put("plan.toml", &plan("\"case0591\""));
    let selection = tree.resolve("plan.toml").unwrap();
    assert_eq!(ids(&selection), ["large:a:case0591"]);
    assert_eq!(
        first(&selection).argv,
        ["/bin/sh", "-c", "printf 'case0591'"]
    );
}

/// 排除掉全部候选或所有步骤都被禁用时，最终集合为空必须报错，避免空计划被当成成功。
#[test]
fn empty_selection_errors_report_the_plan() {
    let tree = Tree::new();
    tree.executable("root/cases/alpha");
    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\ndirectory = \"/cases\"\nexclude = [\"*\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    let error = tree.error("plan.toml");
    assert!(
        error.contains("empty selection") && error.contains("plan.toml"),
        "unexpected error: {error}"
    );

    tree.put(
        "plan.toml",
        &format!(
            "{}[[group]]\nname = \"g\"\nroots = [\"a\"]\n[[group.step]]\nenabled = false\nnames = [\"x\"]\nexec = [\"/x\"]\n",
            tree.roots()
        ),
    );
    let error = tree.error("plan.toml");
    assert!(
        error.contains("empty selection"),
        "unexpected error: {error}"
    );
}

/// 仓库的现场与决赛示例计划保持脚本原样：不追加组标记，超时与解释器按集合配置生效。
#[test]
fn shipped_example_plans_keep_plain_scripts() {
    let contest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../contest");
    let onsite = config::load(&contest.join("onsite-plan.toml"), &[]).unwrap();
    for arch in ["riscv64", "loongarch64"] {
        let selection = selection::resolve(&onsite, arch, &[]).unwrap();
        assert_eq!(selection.groups.len(), 1);
        assert_eq!(
            ids(&selection),
            ["onsite:onsite:task_a.elf", "onsite:onsite:task_b.elf"]
        );
        assert_eq!(selection.groups[0].cases[0].argv, ["./task_a.elf"]);
        assert_eq!(selection.groups[0].cases[1].argv, ["./task_b.elf"]);
    }

    let finals = config::load(&contest.join("final-plan.toml"), &[]).unwrap();
    let selection = selection::resolve(&finals, "riscv64", &[]).unwrap();
    assert_eq!(selection.groups.len(), 2);
    for group in &selection.groups {
        assert!(group.start.is_empty() && group.end.is_empty());
        assert_eq!(group.cases.len(), 1);
        assert_eq!(group.cases[0].name, "script");
    }
    assert_eq!(selection.groups[0].cases[0].argv[0], "/bin/bash");
    assert_eq!(selection.groups[0].cases[0].timeout_sec, 120);
    assert_eq!(selection.groups[1].cases[0].argv[0], "/bin/sh");
    assert_eq!(selection.groups[1].cases[0].timeout_sec, 18000);
}

/// 发布默认计划在同一假盘上展开为 12 个集合、每 root 一个结果组；libctest/basic/ltp 的标记、用例数与一个脚本组互不混淆。
#[test]
fn shipped_default_plan_expands_twelve_groups() {
    const BASIC: [&str; 32] = [
        "brk",
        "chdir",
        "clone",
        "close",
        "dup2",
        "dup",
        "execve",
        "exit",
        "fork",
        "fstat",
        "getcwd",
        "getdents",
        "getpid",
        "getppid",
        "gettimeofday",
        "mkdir_",
        "mmap",
        "mount",
        "munmap",
        "openat",
        "open",
        "pipe",
        "read",
        "sleep",
        "times",
        "umount",
        "uname",
        "unlink",
        "wait",
        "waitpid",
        "write",
        "yield",
    ];
    const SCRIPTS: [&str; 9] = [
        "busybox",
        "lua",
        "iozone",
        "unixbench",
        "iperf",
        "libcbench",
        "lmbench",
        "netperf",
        "cyclictest",
    ];
    const GROUP_ORDER: [&str; 12] = [
        "basic",
        "busybox",
        "lua",
        "libctest",
        "iozone",
        "unixbench",
        "iperf",
        "libcbench",
        "lmbench",
        "netperf",
        "cyclictest",
        "ltp",
    ];

    let tree = Tree::new();
    for name in BASIC {
        tree.executable(&format!("root/basic/{name}"));
    }
    for name in ["open01", "close01", "mmap01", "open_by_handle_at01"] {
        tree.executable(&format!("root/ltp/testcases/bin/{name}"));
    }

    let plan = Path::new(env!("CARGO_MANIFEST_DIR")).join("../contest/plan.toml");
    let mut plan_config = config::load(&plan, &[]).unwrap();
    for root in ["musl", "glibc"] {
        plan_config.roots.get_mut(root).unwrap().path = tree.path("root");
    }
    let selection = selection::resolve(&plan_config, "riscv64", &[]).unwrap();

    assert_eq!(selection.groups.len(), 24);
    for (index, name) in GROUP_ORDER.iter().enumerate() {
        assert_eq!(selection.groups[index * 2].name.as_str(), *name);
        assert_eq!(selection.groups[index * 2].root, "musl");
        assert_eq!(selection.groups[index * 2 + 1].name.as_str(), *name);
        assert_eq!(selection.groups[index * 2 + 1].root, "glibc");
    }

    let pick = |name: &str, root: &str| {
        selection
            .groups
            .iter()
            .find(|group| group.name == name && group.root == root)
            .unwrap()
    };

    for root in ["musl", "glibc"] {
        let basic = pick("basic", root);
        assert_eq!(basic.cases.len(), 32);
        assert_eq!(
            basic.start,
            format!("#### OS COMP TEST GROUP START basic-{root} ####\n")
        );
        assert_eq!(
            basic.end,
            format!("#### OS COMP TEST GROUP END basic-{root} ####\n")
        );

        let libc = pick("libctest", root);
        assert_eq!(libc.cases.len(), 217);
        assert_eq!(libc.cases[0].id, format!("libctest:{root}:dynamic:argv"));
        assert_eq!(libc.cases[110].id, format!("libctest:{root}:static:argv"));
        let dynamic = format!("libctest:{root}:dynamic:");
        let statik = format!("libctest:{root}:static:");
        assert!(libc.cases[..110]
            .iter()
            .all(|case| case.id.starts_with(&dynamic)));
        assert!(libc.cases[110..]
            .iter()
            .all(|case| case.id.starts_with(&statik)));
        assert_eq!(
            libc.start,
            format!("#### OS COMP TEST GROUP START libctest-{root} ####\n")
        );
        assert_eq!(
            libc.end,
            format!("#### OS COMP TEST GROUP END libctest-{root} ####\n")
        );

        let ltp = pick("ltp", root);
        let names: Vec<&str> = ltp.cases.iter().map(|case| case.name.as_str()).collect();
        assert_eq!(names, ["open01", "close01", "mmap01"]);
        assert_eq!(
            ltp.start,
            format!("#### OS COMP TEST GROUP START ltp-{root} ####\n")
        );
        assert_eq!(
            ltp.end,
            format!("#### OS COMP TEST GROUP END ltp-{root} ####\n")
        );

        for name in SCRIPTS {
            let group = pick(name, root);
            assert_eq!(group.cases.len(), 1);
            assert_eq!(group.cases[0].name, "script");
            assert!(group.start.is_empty());
            assert!(group.end.is_empty());
        }
    }

    let basic = pick("basic", "musl");
    let mmap = basic.cases.iter().find(|case| case.name == "mmap").unwrap();
    assert_eq!(mmap.argv, ["./mmap"]);
    assert_eq!(mmap.cwd.as_path(), Path::new("/basic"));
    assert_eq!(mmap.timeout_sec, 120);
    let glibc = pick("basic", "glibc");
    let glibc_mmap = glibc.cases.iter().find(|case| case.name == "mmap").unwrap();
    assert_eq!(glibc_mmap.timeout_sec, 60);
}
