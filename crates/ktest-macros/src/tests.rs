use quote::quote;

use crate::expand;

/// 不支持的函数形状必须在宏边界失败，不能留下假运行入口。
#[test]
fn rejects_unsupported_signatures() {
    let functions = [
        quote!(
            async fn example() {}
        ),
        quote!(
            fn example<T>() {}
        ),
        quote!(
            fn example() -> bool {
                true
            }
        ),
        quote!(
            unsafe fn example() {}
        ),
        quote!(
            extern "C" fn example() {}
        ),
        quote!(
            fn example(value: usize) {}
        ),
        quote!(
            fn example(context: &ktest::TestContext) {}
        ),
        quote!(
            fn example(value: usize, context: &mut ktest::TestContext) {}
        ),
    ];
    for function in functions {
        assert!(expand(quote!(), quote!(mod tests { #[test] #function })).is_err());
    }
}

/// 参数名重名与包含分隔符都会使外部身份产生歧义。
#[test]
fn rejects_ambiguous_parameter_instances() {
    let inputs = [
        quote!(
            mod tests {
                #[test]
                #[case("same", 1)]
                #[case("same", 2)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                #[case("bad[name]", 1)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                #[case("bad::name", 1)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                #[case("bad\nname", 1)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                #[case("", 1)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                #[case("one", 1, 2)]
                fn example(value: usize) {}
            }
        ),
        quote!(
            mod tests {
                #[test]
                fn same() {}
                #[test]
                fn same() {}
            }
        ),
    ];
    for input in inputs {
        assert!(expand(quote!(), input).is_err());
    }
}

/// 重复属性、未知资源和不完整 fatal 条件不能进入静态目录。
#[test]
fn rejects_invalid_metadata() {
    let attributes = [
        quote!(min_cpus = 1, min_cpus = 2),
        quote!(min_cpus = 0),
        quote!(timeout_ms = 0),
        quote!(timeout = 3, timeout_ms = 4),
        quote!(context = "Interrupt"),
        quote!(resources("network")),
        quote!(resources("root", "root")),
        quote!(expected_fatal = "unknown"),
        quote!(expected_fatal = "panic"),
        quote!(fatal_message = "unbound"),
        quote!(expected_fatal = "panic", fatal_message = ""),
    ];
    for attribute in attributes {
        assert!(expand(
            attribute,
            quote!(
                mod tests {
                    #[test]
                    fn example() {}
                }
            )
        )
        .is_err());
    }
    assert!(expand(
        quote!(expected_fatal = "illegal"),
        quote!(
            mod tests {
                #[test]
                #[should_panic]
                fn example() {}
            }
        )
    )
    .is_err());
    assert!(expand(
        quote!(),
        quote!(
            mod tests {
                #[test]
                #[ktest(context = "Boot")]
                fn example() {}
            }
        )
    )
    .is_err());
}

/// 条件编译必须包住 suite，禁止静态描述符引用被删除的单个函数。
#[test]
fn rejects_per_case_conditional_compilation() {
    assert!(expand(
        quote!(),
        quote!(
            mod tests {
                #[test]
                #[cfg(feature = "other")]
                fn example() {}
            }
        )
    )
    .is_err());
}
