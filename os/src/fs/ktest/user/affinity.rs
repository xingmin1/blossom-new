//! 普通用户程序直接消费 affinity/getcpu；执行树沿原 UserOwner 完整回收。

use super::*;

#[ktest::suite(fixture = UserOwner, suite_resources = UserSuite, context = "Thread", tags("user-elf", "user-affinity", "cpu-smp"), resources("root"), timeout_ms = 90000)]
pub(crate) mod behavior {
    use super::*;

    /// 同源 C 检查短长 mask、实际每核迁移、fork/clone 继承及远端 Running 交接，TAP 输出只作注释。
    #[test]
    #[case("root", Credentials::ROOT)]
    #[case("same_uid", Credentials { uid: 1000, gid: 1000 })]
    fn masks_migration_and_getcpu(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
        credentials: Credentials,
    ) -> TestResult {
        fixture.prepare_tty(context, 4)?;
        fixture.run_selftest_named(
            context,
            b"/test/selftests/process/affinity",
            &[],
            credentials,
        )
    }
}
