use std::collections::BTreeSet;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::Parser, punctuated::Punctuated, spanned::Spanned, Expr, FnArg, Item, ItemFn, ItemMod,
    Lit, LitStr, Meta, ReturnType, Token, Type,
};

/// 将内联 suite 模块展开为静态描述符与具体类型的生命周期 wrapper。
#[proc_macro_attribute]
pub fn suite(attributes: TokenStream, input: TokenStream) -> TokenStream {
    match expand(attributes.into(), input.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[derive(Default, Clone)]
struct Options {
    fixture: Option<Type>,
    base: Option<Type>,
    name: Option<LitStr>,
    context: Option<LitStr>,
    isolation: Option<LitStr>,
    tags: Vec<LitStr>,
    resources: Vec<LitStr>,
    min_cpus: Option<u32>,
    timeout_ms: Option<u64>,
    ignore: Option<LitStr>,
    fatal: Option<LitStr>,
    fatal_message: Option<LitStr>,
    seen: BTreeSet<String>,
}

/// 仅接受列出的少量元属性，重复属性在作者位置报错。
impl Options {
    /// 解析 suite 或 case 的静态选项，不解释业务参数表达式。
    fn parse(&mut self, tokens: Tokens, suite: bool) -> syn::Result<()> {
        let entries = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(tokens)?;
        for entry in entries {
            let key = entry
                .path()
                .get_ident()
                .ok_or_else(|| syn::Error::new(entry.span(), "expected a simple attribute name"))?
                .to_string();
            if !self.seen.insert(key.clone()) {
                return Err(syn::Error::new(entry.span(), "duplicate ktest attribute"));
            }
            match (key.as_str(), &entry) {
                ("tags" | "resources", Meta::List(list)) => {
                    let values =
                        list.parse_args_with(Punctuated::<LitStr, Token![,]>::parse_terminated)?;
                    let mut names = BTreeSet::new();
                    for value in values {
                        valid_label(&value)?;
                        if !names.insert(value.value()) {
                            return Err(syn::Error::new(value.span(), "duplicate tag or resource"));
                        }
                        if key == "resources"
                            && !matches!(
                                value.value().as_str(),
                                "uart"
                                    | "scratch"
                                    | "root"
                                    | "extra"
                                    | "ap-freeze"
                                    | "virtio-access-platform"
                            )
                        {
                            return Err(syn::Error::new(
                                value.span(),
                                "resource must be uart, scratch, root, extra, ap-freeze or virtio-access-platform",
                            ));
                        }
                        if key == "tags" {
                            self.tags.push(value);
                        } else {
                            self.resources.push(value);
                        }
                    }
                }
                ("fixture" | "suite_resources", Meta::NameValue(value)) if suite => {
                    let ty: Type = syn::parse2(value.value.to_token_stream())?;
                    if key == "fixture" {
                        self.fixture = Some(ty);
                    } else {
                        self.base = Some(ty);
                    }
                }
                ("name" | "context", Meta::NameValue(value)) if suite => {
                    let text = literal_string(&value.value)?;
                    if key == "name" {
                        valid_label(&text)?;
                        self.name = Some(text);
                    } else {
                        if !matches!(text.value().as_str(), "Boot" | "Thread" | "boot" | "thread") {
                            return Err(syn::Error::new(
                                text.span(),
                                "context must be Boot or Thread",
                            ));
                        }
                        self.context = Some(text);
                    }
                }
                ("isolation", Meta::NameValue(value)) => {
                    let text = literal_string(&value.value)?;
                    if !matches!(text.value().as_str(), "Suite" | "Case" | "suite" | "case") {
                        return Err(syn::Error::new(
                            text.span(),
                            "isolation must be suite or case",
                        ));
                    }
                    self.isolation = Some(text);
                }
                ("ignore" | "expected_fatal" | "fatal_message", Meta::NameValue(value)) => {
                    let text = literal_string(&value.value)?;
                    valid_message(&text)?;
                    match key.as_str() {
                        "ignore" => self.ignore = Some(text),
                        "fatal_message" => self.fatal_message = Some(text),
                        _ => {
                            if !matches!(
                                text.value().as_str(),
                                "illegal"
                                    | "refill"
                                    | "irq-schedule"
                                    | "preempt-schedule"
                                    | "oom"
                                    | "timekeeper"
                                    | "panic"
                                    | "double-panic"
                                    | "abi"
                            ) {
                                return Err(syn::Error::new(
                                    text.span(),
                                    "unknown expected_fatal type",
                                ));
                            }
                            self.fatal = Some(text);
                        }
                    }
                }
                ("min_cpus" | "timeout_ms" | "timeout", Meta::NameValue(value)) => {
                    let Expr::Lit(literal) = &value.value else {
                        return Err(syn::Error::new(
                            value.span(),
                            "expected a positive integer literal",
                        ));
                    };
                    let Lit::Int(integer) = &literal.lit else {
                        return Err(syn::Error::new(
                            value.span(),
                            "expected a positive integer literal",
                        ));
                    };
                    let number: u64 = integer.base10_parse()?;
                    if number == 0 {
                        return Err(syn::Error::new(
                            integer.span(),
                            "budget and CPU count must be positive",
                        ));
                    }
                    if key == "min_cpus" {
                        self.min_cpus = Some(u32::try_from(number).map_err(|_| {
                            syn::Error::new(integer.span(), "CPU count exceeds u32")
                        })?);
                    } else {
                        if self.timeout_ms.is_some() {
                            return Err(syn::Error::new(
                                integer.span(),
                                "timeout and timeout_ms are the same attribute",
                            ));
                        }
                        self.timeout_ms = Some(number);
                    }
                }
                _ => {
                    return Err(syn::Error::new(
                        entry.span(),
                        "unsupported ktest attribute or value shape",
                    ))
                }
            }
        }
        Ok(())
    }
}

/// 元信息只接受字符串字面量，运行时表达式不能改变目录。
fn literal_string(expr: &Expr) -> syn::Result<LitStr> {
    if let Expr::Lit(value) = expr {
        if let Lit::Str(value) = &value.lit {
            return Ok(value.clone());
        }
    }
    Err(syn::Error::new(expr.span(), "expected a string literal"))
}
/// 名字不得含控制字符和全名的歧义分隔符。
fn valid_label(value: &LitStr) -> syn::Result<()> {
    let name = value.value();
    if name.trim().is_empty()
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '[' | ']' | ':'))
    {
        return Err(syn::Error::new(
            value.span(),
            "name must be nonempty without control characters, ':', '[' or ']'",
        ));
    }
    Ok(())
}
/// ignore 与 fatal 信息保持单行且非空。
fn valid_message(value: &LitStr) -> syn::Result<()> {
    if value.value().trim().is_empty() || value.value().chars().any(char::is_control) {
        return Err(syn::Error::new(
            value.span(),
            "message must be nonempty without control characters",
        ));
    }
    Ok(())
}

struct Instance {
    label: Option<LitStr>,
    arguments: Vec<Expr>,
}
struct Test {
    function: ItemFn,
    options: Options,
    panic: Option<LitStr>,
    instances: Vec<Instance>,
    context: bool,
    fixture: bool,
}

/// 消费测试专用属性，保留作者函数及其断言不变。
fn parse_test(mut function: ItemFn, suite: &Options) -> syn::Result<Test> {
    let signature = &function.sig;
    if signature.asyncness.is_some()
        || signature.unsafety.is_some()
        || signature.constness.is_some()
        || signature.abi.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
        || signature.variadic.is_some()
    {
        return Err(syn::Error::new(
            signature.span(),
            "ktest requires a safe, non-generic, synchronous Rust function",
        ));
    }
    match &signature.output {
        ReturnType::Default => (),
        ReturnType::Type(_, ty)
            if matches!(&**ty, Type::Tuple(tuple) if tuple.elems.is_empty())
                || type_name(ty) == Some("TestResult") =>
        {
            ()
        }
        other => {
            return Err(syn::Error::new(
                other.span(),
                "test must return () or TestResult",
            ))
        }
    }
    let mut options = Options::default();
    let mut panic = None;
    let mut panic_seen = false;
    let mut instances = Vec::new();
    let mut retained = Vec::new();
    let mut parameter_names = BTreeSet::new();
    for attribute in function.attrs {
        if attribute.path().is_ident("test") {
            continue;
        }
        if attribute.path().is_ident("case") {
            let args =
                attribute.parse_args_with(Punctuated::<Expr, Token![,]>::parse_terminated)?;
            let mut args = args.into_iter();
            let first = args.next().ok_or_else(|| {
                syn::Error::new(attribute.span(), "case requires a parameter name")
            })?;
            let label = literal_string(&first)?;
            valid_label(&label)?;
            if !parameter_names.insert(label.value()) {
                return Err(syn::Error::new(label.span(), "duplicate parameter name"));
            }
            instances.push(Instance {
                label: Some(label),
                arguments: args.collect(),
            });
        } else if attribute.path().is_ident("ktest") {
            options.parse(attribute.meta.require_list()?.tokens.clone(), false)?;
        } else if attribute.path().is_ident("ignore") {
            if options.ignore.is_some() {
                return Err(syn::Error::new(
                    attribute.span(),
                    "duplicate ignore attribute",
                ));
            }
            let value = match &attribute.meta {
                Meta::NameValue(value) => literal_string(&value.value)?,
                _ => {
                    return Err(syn::Error::new(
                        attribute.span(),
                        "ignore requires a reason: #[ignore = \"reason\"]",
                    ))
                }
            };
            valid_message(&value)?;
            options.ignore = Some(value);
            options.seen.insert("ignore".into());
        } else if attribute.path().is_ident("should_panic") {
            if panic_seen {
                return Err(syn::Error::new(
                    attribute.span(),
                    "duplicate should_panic attribute",
                ));
            }
            panic_seen = true;
            match &attribute.meta {
                Meta::Path(_) => panic = Some(LitStr::new("", attribute.span())),
                Meta::List(list) => {
                    let args =
                        list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
                    if args.len() != 1 {
                        return Err(syn::Error::new(
                            attribute.span(),
                            "should_panic accepts only expected = \"message\"",
                        ));
                    }
                    match args.first().unwrap() {
                        Meta::NameValue(value) if value.path.is_ident("expected") => {
                            panic = Some(literal_string(&value.value)?)
                        }
                        _ => {
                            return Err(syn::Error::new(
                                attribute.span(),
                                "should_panic accepts only expected = \"message\"",
                            ))
                        }
                    }
                }
                _ => {
                    return Err(syn::Error::new(
                        attribute.span(),
                        "invalid should_panic attribute",
                    ))
                }
            }
        } else if attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr") {
            return Err(syn::Error::new(
                attribute.span(),
                "gate the suite module, not an individual registered test",
            ));
        } else {
            retained.push(attribute);
        }
    }
    function.attrs = retained;
    let inputs: Vec<_> = function.sig.inputs.iter().collect();
    let context = inputs
        .first()
        .is_some_and(|arg| is_mut_reference(arg, "TestContext"));
    // 第二个可变借用交给具体 fixture；路径别名的等价性由正常 Rust 类型检查判断。
    let fixture = context
        && suite.fixture.is_some()
        && matches!(inputs.get(1).copied(), Some(FnArg::Typed(arg)) if matches!(&*arg.ty, Type::Reference(reference) if reference.mutability.is_some()));
    let injected = usize::from(context) + usize::from(fixture);
    for arg in &inputs[injected..] {
        if let FnArg::Typed(arg) = arg {
            if type_name(&arg.ty) == Some("TestContext")
                || matches!(&*arg.ty, Type::Reference(reference) if type_name(&reference.elem) == Some("TestContext"))
            {
                return Err(syn::Error::new(
                    arg.span(),
                    "TestContext must be the first &mut argument",
                ));
            }
        } else {
            return Err(syn::Error::new(arg.span(), "test cannot have a receiver"));
        }
    }
    if instances.is_empty() {
        instances.push(Instance {
            label: None,
            arguments: Vec::new(),
        });
    }
    for instance in &instances {
        if instance.arguments.len() != inputs.len() - injected {
            return Err(syn::Error::new(
                function.sig.span(),
                "case expression count does not match static parameters",
            ));
        }
    }
    if panic.is_some() && (options.fatal.is_some() || suite.fatal.is_some()) {
        return Err(syn::Error::new(
            function.sig.span(),
            "should_panic and expected_fatal are mutually exclusive",
        ));
    }
    let fatal = options.fatal.as_ref().or(suite.fatal.as_ref());
    let fatal_message = options
        .fatal_message
        .as_ref()
        .or(suite.fatal_message.as_ref());
    if fatal.is_none() && fatal_message.is_some() {
        return Err(syn::Error::new(
            function.sig.span(),
            "fatal_message requires expected_fatal",
        ));
    }
    if fatal.is_some_and(|value| value.value() == "panic") && fatal_message.is_none() {
        return Err(syn::Error::new(
            function.sig.span(),
            "panic expected_fatal requires a nonempty fatal_message",
        ));
    }
    Ok(Test {
        function,
        options,
        panic,
        instances,
        context,
        fixture,
    })
}

/// 从路径类型取末尾名，用于有限签名识别。
fn type_name(ty: &Type) -> Option<&str> {
    // Ident 字符串不能借出临时值，固定名字直接匹配。
    if let Type::Path(path) = ty {
        let ident = &path.path.segments.last()?.ident;
        if ident == "TestContext" {
            Some("TestContext")
        } else if ident == "TestResult" {
            Some("TestResult")
        } else {
            None
        }
    } else {
        None
    }
}
/// 上下文参数必须是可变借用，生命周期交由 Rust 正常检查。
fn is_mut_reference(arg: &FnArg, name: &str) -> bool {
    matches!(arg, FnArg::Typed(arg) if matches!(&*arg.ty, Type::Reference(reference) if reference.mutability.is_some() && type_name(&reference.elem) == Some(name)))
}

/// 取 suite 与 case 静态集合的并集，不重复目录条目。
fn merged_strings(base: &[LitStr], case: &[LitStr]) -> Vec<LitStr> {
    let mut result = base.to_vec();
    for value in case {
        if !result.iter().any(|old| old.value() == value.value()) {
            result.push(value.clone());
        }
    }
    result
}
/// 属性解析后生成有效默认值，隔离只允许提升。
fn defaults(base: &Options, case: Option<&Options>) -> Tokens {
    let context = if base
        .context
        .as_ref()
        .is_some_and(|value| value.value().eq_ignore_ascii_case("boot"))
    {
        quote!(ktest::Context::Boot)
    } else {
        quote!(ktest::Context::Thread)
    };
    let isolated = base
        .isolation
        .as_ref()
        .is_some_and(|value| value.value().eq_ignore_ascii_case("case"))
        || case.is_some_and(|case| {
            case.isolation
                .as_ref()
                .is_some_and(|value| value.value().eq_ignore_ascii_case("case"))
        })
        || base.fatal.is_some()
        || case.is_some_and(|case| case.fatal.is_some());
    let isolation = if isolated {
        quote!(ktest::Isolation::Case)
    } else {
        quote!(ktest::Isolation::Suite)
    };
    let min_cpus = base
        .min_cpus
        .unwrap_or(1)
        .max(case.and_then(|case| case.min_cpus).unwrap_or(1));
    let timeout = case
        .and_then(|case| case.timeout_ms)
        .or(base.timeout_ms)
        .unwrap_or(10_000);
    let tags = merged_strings(&base.tags, case.map_or(&[], |case| &case.tags));
    let resources = merged_strings(&base.resources, case.map_or(&[], |case| &case.resources));
    let resources = resources.iter().map(|value| match value.value().as_str() {
        "uart" => quote!(ktest::Resource::Uart),
        "scratch" => quote!(ktest::Resource::Scratch),
        "root" => quote!(ktest::Resource::Root),
        "extra" => quote!(ktest::Resource::Extra),
        "ap-freeze" => quote!(ktest::Resource::ApFreeze),
        "virtio-access-platform" => quote!(ktest::Resource::VirtioAccessPlatform),
        _ => unreachable!(),
    });
    quote!(ktest::Defaults { context: #context, isolation: #isolation, tags: &[#(#tags),*], min_cpus: #min_cpus, timeout_ms: #timeout, resources: &[#(#resources),*] })
}
/// 将可选字符串编码为静态 Option。
fn optional(value: Option<&LitStr>) -> Tokens {
    match value {
        Some(value) => quote!(Some(#value)),
        None => quote!(None),
    }
}

/// 为每个静态参数实例生成 wrapper，类型错误仍由正常 Rust 类型检查负责。
fn expand(attributes: Tokens, input: Tokens) -> syn::Result<Tokens> {
    let mut options = Options::default();
    options.parse(attributes, true)?;
    let mut module: ItemMod = syn::parse2(input)?;
    let module_span = module.ident.span();
    let (_, items) = module
        .content
        .as_mut()
        .ok_or_else(|| syn::Error::new(module_span, "suite requires an inline module"))?;
    let mut tests = Vec::new();
    let mut names = BTreeSet::new();
    let mut preserved = Vec::new();
    for item in items.drain(..) {
        if let Item::Fn(function) = item {
            if function
                .attrs
                .iter()
                .any(|attr| attr.path().is_ident("test"))
            {
                if !names.insert(function.sig.ident.to_string()) {
                    return Err(syn::Error::new(
                        function.sig.ident.span(),
                        "duplicate test name",
                    ));
                }
                tests.push(parse_test(function, &options)?);
            } else {
                preserved.push(Item::Fn(function));
            }
        } else {
            preserved.push(item);
        }
    }
    if tests.is_empty() {
        return Err(syn::Error::new(
            module.ident.span(),
            "suite must contain at least one test",
        ));
    }
    let fixture = options
        .fixture
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(()));
    let base = options
        .base
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(()));
    let suite_name = match &options.name {
        Some(name) => quote!(concat!(module_path!(), "::", #name)),
        None => quote!(module_path!()),
    };
    let suite_defaults = defaults(&options, None);
    let mut cases = Vec::new();
    let mut wrappers = Vec::new();
    let mut arms = Vec::new();
    for test in tests {
        let function = &test.function;
        let name = &function.sig.ident;
        let source_span = name.span();
        let attrs = defaults(&options, Some(&test.options));
        let ignore = optional(test.options.ignore.as_ref().or(options.ignore.as_ref()));
        let panic = optional(test.panic.as_ref());
        let fatal = optional(test.options.fatal.as_ref().or(options.fatal.as_ref()));
        let fatal_message = optional(
            test.options
                .fatal_message
                .as_ref()
                .or(options.fatal_message.as_ref()),
        );
        for instance in &test.instances {
            let index = cases.len();
            let wrapper = format_ident!("__ktest_case_{}", index, span = Span::mixed_site());
            let full_name = match &instance.label {
                Some(label) => {
                    quote!(concat!(#suite_name, "::", stringify!(#name), "[", #label, "]"))
                }
                None => quote!(concat!(#suite_name, "::", stringify!(#name))),
            };
            let parameter = optional(instance.label.as_ref());
            let source = quote::quote_spanned!(source_span=> file!());
            let line = quote::quote_spanned!(source_span=> line!());
            cases.push(quote!(ktest::CaseDesc { name: #full_name, parameter: #parameter, source: #source, line: #line, attributes: #attrs, ignore: #ignore, should_panic: #panic, expected_fatal: #fatal, fatal_message: #fatal_message }));
            let mut arguments = Vec::new();
            if test.context {
                arguments.push(quote!(__context));
            }
            if test.fixture {
                arguments.push(quote!(__fixture));
            }
            arguments.extend(instance.arguments.iter().map(ToTokens::to_token_stream));
            wrappers.push(quote!(
                /// 具体 fixture 保留在主体捕获边界外，参数表达式只在主体内求值。
                fn #wrapper(__context: &mut ktest::TestContext<'_>, __resources: &#base) -> ktest::CaseOutcome {
                    ktest::run_case::<#fixture, #base>(__context, __resources, |__context, __fixture| {
                        ktest::IntoTestResult::into_test_result(#name(#(#arguments),*))
                    })
                }
            ));
            arms.push(quote!(#index => #wrapper(__context, __resources)));
        }
        preserved.push(Item::Fn(test.function));
    }
    // export_name 使用真实 crate/module 路径，避免跨 crate 同短名的归档锚点碰撞。
    let generated: syn::File = syn::parse2(quote!(
        #(#wrappers)*
        pub static __KTEST_CASES: &[ktest::CaseDesc] = &[#(#cases),*];
        /// 分派唯一静态实例索引；运行器必须先验证该索引属于当前 suite。
        fn __ktest_dispatch(__context: &mut ktest::TestContext<'_>, __resources: &#base, __index: usize) -> ktest::CaseOutcome {
            match __index { #(#arms,)* _ => panic!("invalid ktest case index") }
        }
        /// suite 基础资源包住本次选择，并由具体类型实施停止和销毁。
        fn __ktest_run(__context: &mut ktest::TestContext<'_>, __runner: &mut dyn ktest::CaseRunner) -> ktest::SuiteOutcome {
            ktest::run_suite::<#base>(__context, __runner, __KTEST_CASES, __ktest_dispatch)
        }
        pub static __KTEST_SUITE: ktest::SuiteDesc = ktest::SuiteDesc {
            name: #suite_name, defaults: #suite_defaults, cases: __KTEST_CASES, run: __ktest_run,
        };
        #[used]
        #[link_section = ".ktest.suites"]
        #[export_name = concat!("__ktest_register__", module_path!())]
        pub static __KTEST_SLOT: ktest::SuiteSlot = ktest::SuiteSlot(&__KTEST_SUITE);
        /// 显式引用本 suite 槽；归档抽取使用统一导出前缀，不依赖 used 代替链接规则。
        #[inline(never)]
        pub fn __ktest_anchor() -> &'static ktest::SuiteSlot { &__KTEST_SLOT }
    ))?;
    preserved.extend(generated.items);
    *items = preserved;
    Ok(quote!(#module))
}

#[cfg(test)]
mod tests;
