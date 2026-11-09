//! 固定字段的单遍模板替换，不构成 Shell 模板引擎。
//!
//! 只有完整命中 `values` 的 `{key}` 被替换：`${...}` 保持原样，未知的 `{x}` 原样保留，插入的值不再展开。
//! 调用方一次传入完整上下文（group、root、name、id、command、status 等），避免先替换 name 再把结果里的
//! status 二次替换。

/// 替换文本中的 `{key}` 占位符，`values` 中不存在的键与 `${...}` 均原样保留。
///
/// 从左到右单遍扫描，按 Unicode 标量前进以保证中文等多字节文本的边界；插入的值直接写入输出，不再参与匹配。
pub fn expand(text: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous = None;
    while !rest.is_empty() {
        // Shell 的 ${name} 不是 runner 的 {name}。
        if rest.starts_with('{') && previous != Some('$') {
            if let Some(close) = rest.find('}') {
                let name = &rest[1..close];
                if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
                    out.push_str(value);
                    rest = &rest[close + 1..];
                    previous = Some('}');
                    continue;
                }
            }
        }
        // 按 Unicode 标量前进，保证中文文本的 UTF-8 边界。
        let ch = rest.chars().next().expect("nonempty input");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
        previous = Some(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::expand;

    /// 字段替换与多字节文本保持正确。
    #[test]
    fn fields_and_unicode() {
        assert_eq!(
            expand(
                "测例 {name} : {status}",
                &[("name", "open01"), ("status", "0")]
            ),
            "测例 open01 : 0"
        );
    }

    /// Shell 变量与普通花括号保持原样，只有 `{name}` 被替换。
    #[test]
    fn preserve_shell_variables_and_literal_braces() {
        assert_eq!(
            expand("echo ${name}; { echo ok; }; {name}", &[("name", "x")]),
            "echo ${name}; { echo ok; }; x"
        );
    }

    /// 插入的清单命令原文不再展开。
    #[test]
    fn do_not_reexpand_inserted_command() {
        let cmd = "echo {name} ${status}";
        assert_eq!(
            expand("{command}", &[("command", cmd), ("name", "wrong")]),
            cmd
        );
    }
}
