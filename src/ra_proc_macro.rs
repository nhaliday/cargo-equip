use anyhow::{anyhow, Context as _};
use cargo_metadata as cm;
use itertools::chain;
use maplit::btreemap;
use ra_ap_paths::AbsPath;
use ra_ap_proc_macro_api::{
    legacy_protocol::msg::PanicMessage, MacroDylib, ProcMacro, ProcMacroClient, ProcMacroKind,
};
use ra_ap_span::{
    Edition, EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SpanData, SyntaxContext, TextSize,
};
use ra_ap_tt::{self as tt, DelimiterKind, Leaf, TopSubtree, TopSubtreeBuilder};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MSRV: Version = Version::new(1, 64, 0);

fn dummy_span() -> Span {
    let file_id = ra_ap_span::FileId::from_raw(0);
    SpanData {
        range: ra_ap_span::TextRange::new(TextSize::from(0), TextSize::from(0)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(file_id, Edition::CURRENT),
            ast_id: ErasedFileAstId::from_raw(0),
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

pub(crate) fn list_proc_macro_dylibs<P: FnMut(&cm::PackageId) -> bool>(
    cargo_messages: &[cm::Message],
    mut filter: P,
) -> BTreeMap<&cm::PackageId, &AbsPath> {
    cargo_messages
        .iter()
        .flat_map(|message| match message {
            cm::Message::CompilerArtifact(artifact) => Some(artifact),
            _ => None,
        })
        .filter(|cm::Artifact { target, .. }| *target.kind == ["proc-macro".to_owned()])
        .filter(|cm::Artifact { package_id, .. }| filter(package_id))
        .flat_map(
            |cm::Artifact {
                 package_id,
                 filenames,
                 ..
             }| {
                filenames
                    .get(0)
                    .map(|filename| (package_id, AbsPath::assert(filename.as_ref())))
            },
        )
        .collect()
}

pub struct ProcMacroExpander<'msg> {
    custom_derive: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
    func_like: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
    attr: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
}

impl<'msg> ProcMacroExpander<'msg> {
    pub(crate) fn spawn(
        proc_macro_srv_exe: &AbsPath,
        dylib_paths: &BTreeMap<&'msg cm::PackageId, &'msg AbsPath>,
    ) -> anyhow::Result<Self> {
        let server = ProcMacroClient::spawn(
            proc_macro_srv_exe,
            std::iter::empty::<(std::ffi::OsString, &Option<std::ffi::OsString>)>(),
        )?;

        let mut custom_derive = btreemap!();
        let mut func_like = btreemap!();
        let mut attr = btreemap!();

        for (&package_id, dylib_path) in dylib_paths {
            let proc_macros = server
                .load_dylib(MacroDylib::new(dylib_path.to_path_buf()))
                .map_err(|e| anyhow!("{}", e))
                .with_context(|| "rust-analyzer error")?;

            for proc_macro in proc_macros {
                match proc_macro.kind() {
                    ProcMacroKind::CustomDerive => &mut custom_derive,
                    ProcMacroKind::Bang => &mut func_like,
                    ProcMacroKind::Attr => &mut attr,
                }
                .insert(proc_macro.name().to_owned(), (package_id, proc_macro));
            }
        }

        Ok(Self {
            custom_derive,
            func_like,
            attr,
        })
    }

    pub(crate) fn macro_names(
        &self,
    ) -> impl Iterator<Item = (&'msg cm::PackageId, BTreeSet<&str>)> {
        let mut names = BTreeMap::<_, BTreeSet<_>>::new();
        for (name, &(pkg, _)) in chain!(&self.custom_derive, &self.func_like, &self.attr) {
            names.entry(pkg).or_default().insert(&**name);
        }
        names.into_iter()
    }

    pub(crate) fn attempt_expand_custom_derive(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::CustomDerive, body, None::<fn() -> _>)
    }

    pub(crate) fn attempt_expand_func_like(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::Bang, body, None::<fn() -> _>)
    }

    pub(crate) fn attempt_expand_attr(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
        attr: impl FnOnce() -> proc_macro2::Group,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::Attr, body, Some(attr))
    }

    fn attempt_expand(
        &self,
        name: &str,
        kind: ProcMacroKind,
        subtree: impl FnOnce() -> proc_macro2::TokenStream,
        attr: Option<impl FnOnce() -> proc_macro2::Group>,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        match kind {
            ProcMacroKind::CustomDerive => &self.custom_derive,
            ProcMacroKind::Bang => &self.func_like,
            ProcMacroKind::Attr => &self.attr,
        }
        .get(name)
        .map(|(_, proc_macro)| {
            let body = from_proc_macro2_group(&proc_macro2::Group::new(
                proc_macro2::Delimiter::None,
                subtree(),
            ));
            let attr_tree = attr.map(|f| from_proc_macro2_group(&f()));
            let span = dummy_span();
            let output = &proc_macro
                .expand(
                    body.view(),
                    attr_tree.as_ref().map(|a| a.view()),
                    vec![],
                    span,
                    span,
                    span,
                    String::new(),
                )
                .map_err(|e| anyhow!("{}", e))
                .with_context(|| "rust-analyzer error")?
                .map_err(|PanicMessage(s)| anyhow!("proc macro paniced: {s:?}"))?;
            Ok(from_ra_top_subtree(output))
        })
        .transpose()
    }
}

fn from_proc_macro2_group(group: &proc_macro2::Group) -> TopSubtree<Span> {
    let span = dummy_span();
    let delimiter = from_proc_macro2_delimiter(group.delimiter(), span);
    let mut builder = TopSubtreeBuilder::new(delimiter);
    for tt in group.stream() {
        add_token_tree(&mut builder, &tt, span);
    }
    builder.build()
}

fn add_token_tree(builder: &mut TopSubtreeBuilder<Span>, tt: &proc_macro2::TokenTree, span: Span) {
    match tt {
        proc_macro2::TokenTree::Group(g) => {
            let kind = from_proc_macro2_delimiter(g.delimiter(), span).kind;
            builder.open(kind, span);
            for child in g.stream() {
                add_token_tree(builder, &child, span);
            }
            builder.close(span);
        }
        proc_macro2::TokenTree::Ident(i) => {
            builder.push(Leaf::from(tt::Ident::new(&i.to_string(), span)));
        }
        proc_macro2::TokenTree::Punct(p) => {
            builder.push(Leaf::from(tt::Punct {
                char: p.as_char(),
                spacing: match p.spacing() {
                    proc_macro2::Spacing::Alone => tt::Spacing::Alone,
                    proc_macro2::Spacing::Joint => tt::Spacing::Joint,
                },
                span,
            }));
        }
        proc_macro2::TokenTree::Literal(l) => {
            builder.push(Leaf::from(tt::token_to_literal(&l.to_string(), span)));
        }
    }
}

fn from_proc_macro2_delimiter(
    delimiter: proc_macro2::Delimiter,
    span: Span,
) -> tt::Delimiter<Span> {
    tt::Delimiter {
        open: span,
        close: span,
        kind: match delimiter {
            proc_macro2::Delimiter::Parenthesis => DelimiterKind::Parenthesis,
            proc_macro2::Delimiter::Brace => DelimiterKind::Brace,
            proc_macro2::Delimiter::Bracket => DelimiterKind::Bracket,
            proc_macro2::Delimiter::None => DelimiterKind::Invisible,
        },
    }
}

fn from_ra_top_subtree(top_subtree: &TopSubtree<Span>) -> proc_macro2::Group {
    let view = top_subtree.view();
    let delimiter = from_ra_delimiter(view.top_subtree().delimiter);
    let stream: proc_macro2::TokenStream = view
        .iter()
        .map(|element| match element {
            tt::iter::TtElement::Subtree(s, iter) => {
                let inner: proc_macro2::TokenStream = iter.map(from_ra_tt_element).collect();
                proc_macro2::TokenTree::Group(proc_macro2::Group::new(
                    from_ra_delimiter(s.delimiter),
                    inner,
                ))
            }
            tt::iter::TtElement::Leaf(leaf) => from_ra_leaf(leaf),
        })
        .collect();
    proc_macro2::Group::new(delimiter, stream)
}

fn from_ra_tt_element(element: tt::iter::TtElement<'_, Span>) -> proc_macro2::TokenTree {
    match element {
        tt::iter::TtElement::Subtree(s, iter) => {
            let inner: proc_macro2::TokenStream = iter.map(from_ra_tt_element).collect();
            proc_macro2::TokenTree::Group(proc_macro2::Group::new(
                from_ra_delimiter(s.delimiter),
                inner,
            ))
        }
        tt::iter::TtElement::Leaf(leaf) => from_ra_leaf(leaf),
    }
}

fn from_ra_delimiter(delimiter: tt::Delimiter<Span>) -> proc_macro2::Delimiter {
    match delimiter.kind {
        DelimiterKind::Parenthesis => proc_macro2::Delimiter::Parenthesis,
        DelimiterKind::Brace => proc_macro2::Delimiter::Brace,
        DelimiterKind::Bracket => proc_macro2::Delimiter::Bracket,
        DelimiterKind::Invisible => proc_macro2::Delimiter::None,
    }
}

fn from_ra_leaf(leaf: &Leaf<Span>) -> proc_macro2::TokenTree {
    match leaf {
        Leaf::Ident(i) => from_ra_ident(i).into(),
        &Leaf::Punct(p) => from_ra_punct(p).into(),
        Leaf::Literal(l) => from_ra_literal(l).into(),
    }
}

fn from_ra_ident(ident: &tt::Ident<Span>) -> proc_macro2::Ident {
    let name = ident.sym.as_str();
    if ident.is_raw.yes() {
        proc_macro2::Ident::new_raw(name, proc_macro2::Span::call_site())
    } else {
        proc_macro2::Ident::new(name, proc_macro2::Span::call_site())
    }
}

fn from_ra_punct(punct: tt::Punct<Span>) -> proc_macro2::Punct {
    let spacing = match punct.spacing {
        tt::Spacing::Alone => proc_macro2::Spacing::Alone,
        tt::Spacing::Joint | tt::Spacing::JointHidden => proc_macro2::Spacing::Joint,
    };
    proc_macro2::Punct::new(punct.char, spacing)
}

fn from_ra_literal(lit: &tt::Literal<Span>) -> proc_macro2::Literal {
    let text = reconstruct_literal_text(lit);
    syn::parse_str(&text)
        .unwrap_or_else(|e| panic!("could not reconstruct literal from {:?}: {}", text, e))
}

fn reconstruct_literal_text(lit: &tt::Literal<Span>) -> String {
    let sym = lit.symbol.as_str();
    let core = match lit.kind {
        tt::LitKind::Str => format!("\"{}\"", sym),
        tt::LitKind::ByteStr => format!("b\"{}\"", sym),
        tt::LitKind::Char => format!("'{}'", sym),
        tt::LitKind::Byte => format!("b'{}'", sym),
        tt::LitKind::Integer | tt::LitKind::Float => sym.to_owned(),
        tt::LitKind::StrRaw(n) => {
            let hashes: String = std::iter::repeat('#').take(n as usize).collect();
            format!("r{hashes}\"{sym}\"{hashes}")
        }
        tt::LitKind::ByteStrRaw(n) => {
            let hashes: String = std::iter::repeat('#').take(n as usize).collect();
            format!("br{hashes}\"{sym}\"{hashes}")
        }
        tt::LitKind::CStr => format!("c\"{sym}\""),
        tt::LitKind::CStrRaw(n) => {
            let hashes: String = std::iter::repeat('#').take(n as usize).collect();
            format!("cr{hashes}\"{sym}\"{hashes}")
        }
        tt::LitKind::Err(_) => sym.to_owned(),
    };
    match &lit.suffix {
        Some(suffix) => format!("{}{}", core, suffix.as_str()),
        None => core,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn roundtrip(input: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        let group = proc_macro2::Group::new(proc_macro2::Delimiter::None, input);
        let ra = from_proc_macro2_group(&group);
        let output = from_ra_top_subtree(&ra);
        output.stream()
    }

    fn assert_roundtrip(input: proc_macro2::TokenStream) {
        let output = roundtrip(input.clone());
        assert_eq!(input.to_string(), output.to_string());
    }

    #[test]
    fn roundtrip_empty() {
        assert_roundtrip(quote! {});
    }

    #[test]
    fn roundtrip_idents() {
        assert_roundtrip(quote! { foo bar baz });
    }

    #[test]
    fn roundtrip_punct() {
        assert_roundtrip(quote! { a + b * c });
        assert_roundtrip(quote! { x -> y => z });
        assert_roundtrip(quote! { a::b::c });
    }

    #[test]
    fn roundtrip_literals() {
        assert_roundtrip(quote! { 42 });
        assert_roundtrip(quote! { 3.14 });
        assert_roundtrip(quote! { "hello world" });
        assert_roundtrip(quote! { b"bytes" });
        assert_roundtrip(quote! { 'c' });
        assert_roundtrip(quote! { 100u64 });
        assert_roundtrip(quote! { 1.0f32 });
    }

    #[test]
    fn roundtrip_delimiters() {
        assert_roundtrip(quote! { (a, b, c) });
        assert_roundtrip(quote! { [1, 2, 3] });
        assert_roundtrip(quote! { { let x = 1; } });
    }

    #[test]
    fn roundtrip_nested() {
        assert_roundtrip(quote! {
            fn main() {
                let v = vec![1, 2, 3];
                println!("{:?}", v);
            }
        });
    }

    #[test]
    fn roundtrip_struct_with_derive() {
        assert_roundtrip(quote! {
            #[derive(Debug, Clone)]
            struct Foo {
                x: i32,
                y: String,
            }
        });
    }

    #[test]
    fn roundtrip_keywords() {
        assert_roundtrip(quote! {
            pub async fn example(self: &Self) -> impl Iterator<Item = u32> {
                if true { loop { break; } } else { match x { _ => {} } }
            }
        });
    }

    #[test]
    fn roundtrip_raw_idents() {
        let r#type = proc_macro2::Ident::new_raw("type", proc_macro2::Span::call_site());
        let r#match = proc_macro2::Ident::new_raw("match", proc_macro2::Span::call_site());
        assert_roundtrip(quote! { let #r#type = #r#match; });
    }

    #[test]
    fn roundtrip_invisible_delimiter() {
        let inner = quote! { a + b };
        let group = proc_macro2::Group::new(proc_macro2::Delimiter::None, inner);
        assert_roundtrip(proc_macro2::TokenTree::Group(group).into());
    }

    #[test]
    fn roundtrip_raw_strings() {
        let raw_str: proc_macro2::TokenStream = r###"r#"hello "world""#"###.parse().unwrap();
        assert_roundtrip(raw_str);
        let raw_bstr: proc_macro2::TokenStream = r###"br#"hello "bytes""#"###.parse().unwrap();
        assert_roundtrip(raw_bstr);
    }

    // --- Expansion integration tests ---

    static BUILD_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

    fn proc_macro_srv_toolchain() -> String {
        std::env::var("CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN")
            .unwrap_or_else(|_| "nightly".to_owned())
    }

    fn solutions_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("solutions")
    }

    /// Build test solutions and return the path to the proconio-derive dylib.
    fn build_and_find_proconio_derive_dylib() -> std::path::PathBuf {
        let _lock = BUILD_LOCK.lock().unwrap();
        let toolchain = proc_macro_srv_toolchain();
        let solutions = solutions_dir();

        let output = std::process::Command::new("rustup")
            .args(&["run", &toolchain, "cargo", "build", "--message-format=json"])
            .current_dir(&solutions)
            .output()
            .expect("failed to run cargo build");

        assert!(
            output.status.success(),
            "cargo build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8(output.stdout).unwrap();
        for line in stdout.lines() {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) {
                if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact")
                    && msg
                        .get("target")
                        .and_then(|t| t.get("kind"))
                        .and_then(|k| k.as_array())
                        .map_or(false, |kinds| {
                            kinds.iter().any(|k| k.as_str() == Some("proc-macro"))
                        })
                    && msg
                        .get("target")
                        .and_then(|t| t.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|n| n == "proconio-derive" || n == "proconio_derive")
                        == Some(true)
                {
                    if let Some(filenames) = msg.get("filenames").and_then(|f| f.as_array()) {
                        if let Some(path) = filenames.first().and_then(|f| f.as_str()) {
                            return std::path::PathBuf::from(path);
                        }
                    }
                }
            }
        }
        panic!("could not find proconio-derive dylib in cargo build output");
    }

    fn find_proc_macro_srv() -> ra_ap_paths::AbsPathBuf {
        let toolchain = proc_macro_srv_toolchain();
        let solutions = solutions_dir();
        crate::toolchain::find_rust_analyzer_proc_macro_srv(
            camino::Utf8Path::from_path(solutions.as_ref()).unwrap(),
            &toolchain,
        )
        .expect("could not find rust-analyzer-proc-macro-srv")
    }

    fn make_expander() -> ProcMacroExpander<'static> {
        // Leak to get 'static lifetime for the test PackageId and AbsPath
        let dylib_path = build_and_find_proconio_derive_dylib();
        let abs_dylib: &'static AbsPath = AbsPath::assert(
            camino::Utf8Path::from_path(Box::leak(dylib_path.into_boxed_path())).unwrap(),
        );
        let package_id: &'static cm::PackageId = Box::leak(Box::new(cm::PackageId {
            repr: "proconio-derive 0.2.1 (registry+https://github.com/rust-lang/crates.io-index)"
                .to_owned(),
        }));

        let srv = find_proc_macro_srv();
        let mut dylib_paths = BTreeMap::new();
        dylib_paths.insert(package_id, abs_dylib);

        ProcMacroExpander::spawn(srv.as_ref(), &dylib_paths)
            .expect("failed to spawn ProcMacroExpander")
    }

    #[test]
    fn expand_fastout_attr() {
        let mut expander = make_expander();

        let result = expander
            .attempt_expand_attr(
                "fastout",
                || {
                    quote! {
                        fn main() {
                            println!("hello");
                        }
                    }
                },
                || {
                    // Match how cargo-equip passes attrs: invisible delimiter, empty stream
                    // for attributes with no arguments like #[fastout]
                    proc_macro2::Group::new(
                        proc_macro2::Delimiter::None,
                        proc_macro2::TokenStream::new(),
                    )
                },
            )
            .expect("expansion failed");

        let expanded = result.expect("fastout should be a known attr macro");
        let code = expanded.stream().to_string();
        // fastout wraps the body in a BufWriter for fast stdout
        assert!(
            code.contains("BufWriter"),
            "expected fastout to produce BufWriter, got: {}",
            code
        );
    }

    #[test]
    fn expand_unknown_macro_returns_none() {
        let mut expander = make_expander();

        let result = expander
            .attempt_expand_func_like("nonexistent_macro", || quote! { foo })
            .expect("should not error");

        assert!(result.is_none(), "unknown macro should return None");
    }

    #[test]
    fn expander_lists_proconio_macros() {
        let expander = make_expander();
        let all_names: BTreeSet<&str> = expander
            .macro_names()
            .flat_map(|(_, names)| names)
            .collect();

        assert!(
            all_names.contains("fastout"),
            "expected fastout in macro names, got: {:?}",
            all_names
        );
        assert!(
            all_names.contains("derive_readable"),
            "expected derive_readable in macro names, got: {:?}",
            all_names
        );
    }
}
