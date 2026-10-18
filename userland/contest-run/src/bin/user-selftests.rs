//! 独立 selftests 入口；返回原调用者，不请求系统关机。

use contest_run::selftests::{self, Options};
use std::io;

/// 解析并执行已安装清单；入口和输出错误统一返回2。
fn main() {
    let result = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "CLI requires UTF-8 arguments".to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .and_then(|args| Options::parse(&args))
        .and_then(|options| selftests::run(&options, &mut io::stdout(), [1, 2]));
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            let _ = std::io::Write::write_fmt(
                &mut io::stderr(),
                format_args!("SELFTEST error: {error}\n"),
            );
            2
        }
    };
    std::process::exit(status);
}
