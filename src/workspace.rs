mod license;

use crate::{process::ProcessBuilderExt as _, shell::Shell, toolchain, User};
use anyhow::{anyhow, bail, Context as _};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata as cm;
use cargo_util::ProcessBuilder;
use if_chain::if_chain;
use indoc::indoc;
use itertools::Itertools as _;
use krates::PkgSpec;
use rand::Rng as _;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env,
    io::Cursor,
    path::{Path, PathBuf},
    str,
};
use strum::EnumString;

pub(crate) fn locate_project(cwd: &Path) -> anyhow::Result<PathBuf> {
    cwd.ancestors()
        .map(|p| p.join("Cargo.toml"))
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "could not find `Cargo.toml` in `{}` or any parent directory",
                cwd.display(),
            )
        })
}

pub(crate) fn cargo_metadata(manifest_path: &Path, cwd: &Path) -> cm::Result<cm::Metadata> {
    cm::MetadataCommand::new()
        .manifest_path(manifest_path)
        .current_dir(cwd)
        .exec()
}

// Run `cargo metadata` against a copy of `manifest_path` with `[dev-dependencies]`
// (and `[target.<cfg>.dev-dependencies]`) stripped, so the returned `resolve` does
// not unify dev-only features into the normal-build feature set. Used when the
// chosen target is not an example/test/bench, where dev-deps wouldn't actually
// be linked.
//
// The strip is done by editing the manifest in place under a Drop guard that
// restores the original on return (including panic). Only the root manifest is
// touched; in workspace setups, dev-deps from sibling members can still leak
// features into the unified resolve.
pub(crate) fn cargo_metadata_excluding_dev_deps(
    manifest_path: &Path,
    cwd: &Path,
) -> anyhow::Result<cm::Metadata> {
    let original = cargo_util::paths::read(manifest_path)?;
    let mut doc = original.parse::<toml_edit::Document>()?;

    let mut changed = doc.as_table_mut().remove("dev-dependencies").is_some();
    if let toml_edit::Item::Table(target_table) = &mut doc["target"] {
        for (_, item) in target_table.iter_mut() {
            if let toml_edit::Item::Table(tbl) = item {
                changed |= tbl.remove("dev-dependencies").is_some();
            }
        }
    }

    if !changed {
        return cargo_metadata(manifest_path, cwd).map_err(Into::into);
    }

    struct Restore {
        path: PathBuf,
        content: String,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Err(e) = cargo_util::paths::write(&self.path, &self.content) {
                eprintln!(
                    "warning: failed to restore manifest `{}`: {}",
                    self.path.display(),
                    e,
                );
            }
        }
    }

    cargo_util::paths::write(manifest_path, doc.to_string())?;
    let _restore = Restore {
        path: manifest_path.to_path_buf(),
        content: original,
    };

    cargo_metadata(manifest_path, cwd).map_err(Into::into)
}

pub(crate) fn resolve_behavior(
    package: &cm::Package,
    workspace_root: &Utf8Path,
) -> anyhow::Result<ResolveBehavior> {
    let cargo_toml = &cargo_util::paths::read(workspace_root.join("Cargo.toml").as_ref())?;
    let CargoToml { workspace } = toml::from_str(cargo_toml)?;
    return Ok(workspace
        .resolver
        .unwrap_or_else(|| package.edition().default_resolver_behavior()));

    #[derive(Deserialize)]
    struct CargoToml {
        #[serde(default)]
        workspace: Workspace,
    }

    #[derive(Default, Deserialize)]
    struct Workspace {
        resolver: Option<ResolveBehavior>,
    }
}

pub(crate) fn cargo_check_message_format_json(
    toolchain: &str,
    metadata: &cm::Metadata,
    package: &cm::Package,
    krate: &cm::Target,
    shell: &mut Shell,
) -> anyhow::Result<Vec<cm::Message>> {
    let messages = ProcessBuilder::new(toolchain::rustup_exe(package.manifest_dir())?)
        .arg("run")
        .arg(toolchain)
        .arg("cargo")
        .arg("check")
        .arg("--message-format")
        .arg("json")
        .arg("-p")
        .arg(format!("{}:{}", package.name, package.version))
        .args(&krate.target_option())
        .cwd(&metadata.workspace_root)
        .try_inspect(|this| shell.status("Running", this))?
        .read_stdout::<Vec<u8>>()?;

    // TODO: check if ≧ 1.41.0

    cm::Message::parse_stream(Cursor::new(messages))
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

pub(crate) fn list_out_dirs<'cm>(
    metadata: &'cm cm::Metadata,
    messages: &[cm::Message],
) -> BTreeMap<&'cm cm::PackageId, Utf8PathBuf> {
    messages
        .iter()
        .flat_map(|message| match message {
            cm::Message::BuildScriptExecuted(cm::BuildScript {
                package_id,
                out_dir,
                ..
            }) => Some((&metadata[package_id].id, out_dir.clone())),
            _ => None,
        })
        .collect()
}

pub(crate) struct TempPkg {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) crate_name: String,
}

impl TempPkg {
    pub(crate) fn manifest_path(&self) -> std::path::PathBuf {
        self.dir.path().join("Cargo.toml")
    }

    pub(crate) fn bundle_path(&self) -> std::path::PathBuf {
        self.dir.path().join(format!("{}.rs", self.crate_name))
    }
}

/// Build a self-contained temporary package mirroring `package`/`target`, with
/// `code` as the single source file. Used to give cargo (or a cargo-plugin
/// like cargo-minify) a buildable project that reflects the bundled output.
pub(crate) fn create_temp_pkg(
    metadata: &cm::Metadata,
    package: &cm::Package,
    target: &cm::Target,
    exclude: &[PkgSpec],
    code: &str,
    prefix: &str,
) -> anyhow::Result<TempPkg> {
    let package_name = {
        let mut rng = rand::thread_rng();
        let suf = (0..16)
            .map(|_| match rng.gen_range(0..=35) {
                n @ 0..=25 => b'a' + n,
                n @ 26..=35 => b'0' + n - 26,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        let suf = str::from_utf8(&suf).expect("should be valid ASCII");
        format!("{}-{}", prefix, suf)
    };
    let crate_name = if target.is_lib() {
        package_name.replace('-', "_")
    } else {
        package_name.clone()
    };

    let temp_pkg = tempfile::Builder::new()
        .prefix(&package_name)
        .rand_bytes(0)
        .tempdir()?;

    let orig_manifest =
        cargo_util::paths::read(package.manifest_path.as_ref())?.parse::<toml_edit::Document>()?;

    let mut temp_manifest = indoc! {r#"
        [package]
        name = ""
        version = "0.0.0"
        edition = ""
    "#}
    .parse::<toml_edit::Document>()
    .unwrap();

    temp_manifest["package"]["name"] = toml_edit::value(&package_name);
    temp_manifest["package"]["edition"] = toml_edit::value(&*package.edition);
    let mut tbl = toml_edit::Table::new();
    tbl["name"] = toml_edit::value(&crate_name);
    tbl["path"] = toml_edit::value(format!("{}.rs", crate_name));
    if target.is_lib() {
        temp_manifest["lib"] = toml_edit::Item::Table(tbl);
    } else {
        temp_manifest[if target.is_example() {
            "example"
        } else {
            "bin"
        }] = toml_edit::Item::ArrayOfTables({
            let mut arr = toml_edit::ArrayOfTables::new();
            arr.push(tbl);
            arr
        });
    }
    temp_manifest["dependencies"] = orig_manifest["dependencies"].clone();
    temp_manifest["dev-dependencies"] = orig_manifest["dev-dependencies"].clone();

    let renames = package
        .dependencies
        .iter()
        .filter(|cm::Dependency { kind, .. }| {
            [cm::DependencyKind::Normal, cm::DependencyKind::Development].contains(kind)
        })
        .flat_map(|cm::Dependency { rename, .. }| rename)
        .collect::<HashSet<_>>();

    let modify_dependencies = |table: &mut toml_edit::Table| {
        for name_in_toml in metadata
            .resolve
            .as_ref()
            .expect("`resolve` is `null`")
            .nodes
            .iter()
            .find(|cm::Node { id, .. }| *id == package.id)
            .expect("should contain")
            .deps
            .iter()
            .filter(|cm::NodeDep { pkg, .. }| !exclude.iter().any(|s| s.matches(&metadata[pkg])))
            .map(|cm::NodeDep { name, pkg, .. }| {
                if renames.contains(&name) {
                    name
                } else {
                    &metadata[pkg].name
                }
            })
        {
            table.remove(name_in_toml);
        }

        for (_, value) in table.iter_mut() {
            if !value["path"].is_none() {
                if let toml_edit::Item::Value(value) = &mut value["path"] {
                    if let Some(possibly_rel_path) = value.as_str() {
                        *value = package
                            .manifest_dir()
                            .join(possibly_rel_path)
                            .into_string()
                            .into();
                    }
                }
            }
        }
    };

    if let toml_edit::Item::Table(table) = &mut temp_manifest["dependencies"] {
        modify_dependencies(table);
    }
    if let toml_edit::Item::Table(table) = &mut temp_manifest["dev-dependencies"] {
        modify_dependencies(table);
    }

    // Add excluded crates that appear anywhere in the resolve graph as dependencies
    // in the check manifest. This handles the case where a bundled library uses an
    // excluded crate transitively. Without this, the check would fail because the
    // bundled code references the excluded crate but it's not in the manifest.
    {
        let all_resolved_pkg_ids: HashSet<&cm::PackageId> = metadata
            .resolve
            .as_ref()
            .into_iter()
            .flat_map(|r| &r.nodes)
            .map(|n| &n.id)
            .collect();

        let deps_table = temp_manifest["dependencies"]
            .as_table_mut()
            .expect("`[dependencies]` should be a table");

        // Collect package names already present in the deps table, including
        // renamed deps where the real package name is in the `package` field.
        let existing_packages: HashSet<String> = deps_table
            .iter()
            .map(|(key, value)| value["package"].as_str().unwrap_or(key).to_owned())
            .collect();

        // Collect features requested on each excluded crate by any resolved package.
        let mut excluded_features: HashMap<&str, (BTreeSet<String>, bool)> = HashMap::new();
        for pkg in &metadata.packages {
            if !all_resolved_pkg_ids.contains(&pkg.id) {
                continue;
            }
            for dep in &pkg.dependencies {
                let (feats, needs_default) = excluded_features
                    .entry(&dep.name)
                    .or_insert_with(|| (BTreeSet::new(), false));
                feats.extend(dep.features.iter().cloned());
                if dep.uses_default_features {
                    *needs_default = true;
                }
            }
        }

        for pkg in &metadata.packages {
            if !all_resolved_pkg_ids.contains(&pkg.id) {
                continue;
            }
            if !exclude.iter().any(|s| s.matches(pkg)) {
                continue;
            }
            if existing_packages.contains(&pkg.name) {
                continue;
            }

            let (features, uses_default_features) = excluded_features
                .get(pkg.name.as_str())
                .map(|(f, d)| (f.clone(), *d))
                .unwrap_or_default();
            let needs_table = !features.is_empty() || !uses_default_features;

            match &pkg.source {
                Some(src) if src.is_crates_io() => {
                    if needs_table {
                        let mut dep = toml_edit::InlineTable::new();
                        dep.insert("version", format!("={}", pkg.version).into());
                        if !uses_default_features {
                            dep.insert("default-features", false.into());
                        }
                        if !features.is_empty() {
                            let arr = features
                                .iter()
                                .map(|f| f.as_str())
                                .collect::<toml_edit::Array>();
                            dep.insert("features", arr.into());
                        }
                        deps_table[&pkg.name] =
                            toml_edit::Item::Value(toml_edit::Value::InlineTable(dep));
                    } else {
                        deps_table[&pkg.name] = toml_edit::value(format!("={}", pkg.version));
                    }
                }
                Some(src) if src.repr.starts_with("git+") => {
                    let url = &src.repr["git+".len()..];
                    let url = url.split(&['?', '#'][..]).next().unwrap_or(url);
                    let mut dep = toml_edit::InlineTable::new();
                    dep.insert("git", url.into());
                    dep.insert("version", format!("={}", pkg.version).into());
                    if !uses_default_features {
                        dep.insert("default-features", false.into());
                    }
                    if !features.is_empty() {
                        let arr = features
                            .iter()
                            .map(|f| f.as_str())
                            .collect::<toml_edit::Array>();
                        dep.insert("features", arr.into());
                    }
                    deps_table[&pkg.name] =
                        toml_edit::Item::Value(toml_edit::Value::InlineTable(dep));
                }
                Some(src) => {
                    bail!(
                        "excluded crate `{}` has unsupported source `{}` for the check manifest",
                        pkg.name,
                        src.repr,
                    );
                }
                None => {
                    let mut dep = toml_edit::InlineTable::new();
                    dep.insert(
                        "path",
                        pkg.manifest_path
                            .parent()
                            .expect("manifest should have parent")
                            .as_str()
                            .into(),
                    );
                    if !uses_default_features {
                        dep.insert("default-features", false.into());
                    }
                    if !features.is_empty() {
                        let arr = features
                            .iter()
                            .map(|f| f.as_str())
                            .collect::<toml_edit::Array>();
                        dep.insert("features", arr.into());
                    }
                    deps_table[&pkg.name] =
                        toml_edit::Item::Value(toml_edit::Value::InlineTable(dep));
                }
            }
        }
    }

    let temp_pkg = TempPkg {
        dir: temp_pkg,
        crate_name,
    };

    cargo_util::paths::write(temp_pkg.manifest_path(), temp_manifest.to_string())?;
    cargo_util::paths::copy(
        metadata.workspace_root.join("Cargo.lock"),
        temp_pkg.dir.path().join("Cargo.lock"),
    )?;
    cargo_util::paths::write(temp_pkg.bundle_path(), code)?;

    Ok(temp_pkg)
}

pub(crate) fn cargo_minify_pass(
    metadata: &cm::Metadata,
    package: &cm::Package,
    target: &cm::Target,
    exclude: &[PkgSpec],
    code: &str,
) -> anyhow::Result<String> {
    let cargo_minify = which::which("cargo-minify").map_err(|_| {
        anyhow!("command not found: cargo-minify (install with `cargo install cargo-minify`)")
    })?;

    let temp_pkg = create_temp_pkg(
        metadata,
        package,
        target,
        exclude,
        code,
        "cargo-equip-minify-input",
    )?;

    // cargo-minify matches rustc-emitted file paths (which are realpath'd via
    // symlink resolution) against the project sources it knows about. On macOS
    // the system tempdir lives under `/var/folders/...` (a symlink to
    // `/private/var/folders/...`), so an unresolved path silently yields zero
    // applicable diagnostics. Invoke with the canonical temp-pkg path as both
    // cwd and `--manifest-path` to keep the comparison consistent.
    let canonical_pkg = std::fs::canonicalize(temp_pkg.dir.path())?;
    let canonical_manifest = std::fs::canonicalize(temp_pkg.manifest_path())?;

    // Capture cargo-minify's stdout and replay it on stderr; otherwise it
    // would mingle with the bundle text on cargo-equip's own stdout, breaking
    // pipes to `pbcopy`, `bat`, etc.
    let stdout: String = ProcessBuilder::new(cargo_minify)
        .args(&["minify", "--apply", "--allow-no-vcs", "--allow-dirty"])
        .arg("--manifest-path")
        .arg(canonical_manifest)
        .cwd(&canonical_pkg)
        .read_stdout()?;
    eprint!("{}", stdout);

    cargo_util::paths::read(&temp_pkg.bundle_path())
}

pub(crate) fn cargo_check_using_current_lockfile_and_cache(
    metadata: &cm::Metadata,
    package: &cm::Package,
    target: &cm::Target,
    exclude: &[PkgSpec],
    code: &str,
) -> anyhow::Result<()> {
    let temp_pkg = create_temp_pkg(
        metadata,
        package,
        target,
        exclude,
        code,
        "cargo-equip-check-output",
    )?;

    ProcessBuilder::new(crate::process::cargo_exe()?)
        .arg("check")
        .arg("--target-dir")
        .arg(&metadata.target_directory)
        .arg("--manifest-path")
        .arg(temp_pkg.manifest_path())
        .args(&if target.is_bin() {
            vec!["--bin", &temp_pkg.crate_name]
        } else if target.is_example() {
            vec!["--example", &temp_pkg.crate_name]
        } else {
            vec!["--lib"]
        })
        .arg("--offline")
        .cwd(&metadata.workspace_root)
        .exec()?;

    Ok(())
}

pub(crate) trait MetadataExt {
    fn exactly_one_target(&self) -> anyhow::Result<(&cm::Target, &cm::Package)>;
    fn lib_target(&self) -> anyhow::Result<(&cm::Target, &cm::Package)>;
    fn bin_target_by_name<'a>(
        &'a self,
        name: &str,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)>;
    fn example_target_by_name<'a>(
        &'a self,
        name: &str,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)>;
    fn target_by_src_path<'a>(
        &'a self,
        src_path: &Path,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)>;
    fn libs_to_bundle<'a>(
        &'a self,
        package_id: &'a cm::PackageId,
        need_dev_deps: bool,
        cargo_udeps_outcome: &HashSet<String>,
        exclude: &[PkgSpec],
    ) -> anyhow::Result<BTreeMap<&'a cm::PackageId, (&'a cm::Target, String)>>;
    fn dep_lib_by_extern_crate_name(
        &self,
        package_id: &cm::PackageId,
        extern_crate_name: &str,
    ) -> Option<&cm::Package>;
    fn libs_with_extern_crate_names(
        &self,
        package_id: &cm::PackageId,
        only: &HashSet<&cm::PackageId>,
    ) -> anyhow::Result<BTreeMap<&cm::PackageId, String>>;
}

impl MetadataExt for cm::Metadata {
    fn exactly_one_target(&self) -> anyhow::Result<(&cm::Target, &cm::Package)> {
        let root_package = self.root_package();
        match (
            &*targets_in_ws(self)
                .filter(|(t, p)| {
                    (t.is_lib() || t.is_bin() || t.is_example())
                        && root_package.map_or(true, |r| r.id == p.id)
                })
                .collect::<Vec<_>>(),
            root_package,
        ) {
            ([], Some(root_package)) => {
                bail!("no lib/bin/example target in `{}`", root_package.name)
            }
            ([], None) => bail!("no lib/bin/example target in this workspace"),
            ([t], _) => Ok(*t),
            ([ts @ ..], _) => bail!(
                "could not determine which target to choose. Use the `--bin` option, `--example` \
                 option, `--lib` option, or `--src` option to specify a target.\n\
                 available targets: {}\n\
                 note: currently `cargo-equip` does not support the `default-run` manifest key.",
                ts.iter()
                    .map(|(target, _)| format!(
                        "{}{}",
                        &target.name,
                        if target.is_lib() {
                            " (lib)"
                        } else if target.is_bin() {
                            " (bin)"
                        } else if target.is_example() {
                            " (example)"
                        } else {
                            unreachable!()
                        }
                    ))
                    .format(", "),
            ),
        }
    }

    fn lib_target(&self) -> anyhow::Result<(&cm::Target, &cm::Package)> {
        let root_package = self.root_package();
        match (
            &*targets_in_ws(self)
                .filter(|(t, p)| t.is_lib() && root_package.map_or(true, |r| r.id == p.id))
                .collect::<Vec<_>>(),
            root_package,
        ) {
            ([], Some(root_package)) => {
                bail!("`{}` does not have a `lib` target", root_package.name)
            }
            ([], None) => bail!("no lib target in this workspace"),
            ([t], _) => Ok(*t),
            ([..], _) => bail!(
                "could not determine which library to choose. Use the `-p` option to specify a \
                 package.",
            ),
        }
    }

    fn bin_target_by_name<'a>(
        &'a self,
        name: &str,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
        target_by_kind_and_name(self, "bin", name)
    }

    fn example_target_by_name<'a>(
        &'a self,
        name: &str,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
        target_by_kind_and_name(self, "example", name)
    }

    fn target_by_src_path<'a>(
        &'a self,
        src_path: &Path,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
        match *targets_in_ws(self)
            .filter(|(t, _)| t.src_path == src_path)
            .collect::<Vec<_>>()
        {
            [] => bail!(
                "`{}` is not the main source file of any bin targets in this workspace ",
                src_path.display(),
            ),
            [bin] => Ok(bin),
            [..] => bail!(
                "multiple bin targets which `src_path` is `{}`",
                src_path.display(),
            ),
        }
    }

    fn libs_to_bundle<'a>(
        &'a self,
        package_id: &'a cm::PackageId,
        need_dev_deps: bool,
        cargo_udeps_outcome: &HashSet<String>,
        exclude: &[PkgSpec],
    ) -> anyhow::Result<BTreeMap<&'a cm::PackageId, (&'a cm::Target, String)>> {
        let package = &self[package_id];

        let renames = package
            .dependencies
            .iter()
            .filter(|cm::Dependency { kind, .. }| {
                [cm::DependencyKind::Normal, cm::DependencyKind::Development].contains(kind)
            })
            .flat_map(|cm::Dependency { rename, .. }| rename)
            .collect::<HashSet<_>>();

        let preds = {
            let rustc_exe = crate::process::cargo_exe()?
                .with_file_name("rustc")
                .with_extension(env::consts::EXE_EXTENSION);

            ProcessBuilder::new(rustc_exe)
                .args(&["--print", "cfg"])
                .cwd(package.manifest_path.with_file_name(""))
                .read_stdout::<String>()?
                .lines()
                .flat_map(cfg_expr::Expression::parse) // https://github.com/EmbarkStudios/cfg-expr/blob/25290dba689ce3f3ab589926ba545875f048c130/src/expr/parser.rs#L180-L195
                .collect::<Vec<_>>()
        };
        let preds = preds
            .iter()
            .flat_map(cfg_expr::Expression::predicates)
            .collect::<Vec<_>>();

        let cm::Resolve { nodes, .. } = self
            .resolve
            .as_ref()
            .with_context(|| "`resolve` is `null`")?;
        let nodes = nodes.iter().map(|n| (&n.id, n)).collect::<HashMap<_, _>>();

        let satisfies = |node_dep: &cm::NodeDep, accepts_dev: bool| -> _ {
            if exclude.iter().any(|s| s.matches(&self[&node_dep.pkg])) {
                return false;
            }

            let cm::Node { features, .. } = &nodes[&node_dep.pkg];
            let features = features.iter().map(|s| &**s).collect::<HashSet<_>>();

            node_dep
                .dep_kinds
                .iter()
                .any(|cm::DepKindInfo { kind, target, .. }| {
                    (*kind == cm::DependencyKind::Normal
                        || accepts_dev && *kind == cm::DependencyKind::Development)
                        && target
                            .as_ref()
                            .and_then(|target| {
                                cfg_expr::Expression::parse(&target.to_string()).ok()
                            })
                            .map_or(true, |target| {
                                target.eval(|pred| match pred {
                                    cfg_expr::Predicate::Feature(feature) => {
                                        features.contains(feature)
                                    }
                                    pred => preds.contains(pred),
                                })
                            })
                })
        };

        if nodes[package_id]
            .deps
            .iter()
            .any(|cm::NodeDep { dep_kinds, .. }| dep_kinds.is_empty())
        {
            bail!("this tool requires Rust 1.41+ for calculating dependencies");
        }

        let mut deps = nodes[package_id]
            .deps
            .iter()
            .filter(|node_dep| satisfies(node_dep, need_dev_deps))
            .flat_map(|node_dep| {
                let lib_package = &self[&node_dep.pkg];
                let lib_target =
                    lib_package.targets.iter().find(|cm::Target { kind, .. }| {
                        *kind == ["lib".to_owned()] || *kind == ["proc-macro".to_owned()]
                    })?;
                let (lib_extern_crate_name, lib_name_in_toml) = if renames.contains(&node_dep.name)
                {
                    (node_dep.name.clone(), &node_dep.name)
                } else {
                    (lib_target.crate_name(), &lib_package.name)
                };
                if cargo_udeps_outcome.contains(lib_name_in_toml) {
                    return None;
                }
                Some((&lib_package.id, (lib_target, lib_extern_crate_name)))
            })
            .chain(
                package
                    .lib_like_target()
                    .map(|lib_target| (package_id, (lib_target, lib_target.crate_name()))),
            )
            .collect::<BTreeMap<_, _>>();

        let all_package_ids = &mut deps.keys().copied().collect::<HashSet<_>>();
        let all_extern_crate_names = &mut deps
            .values()
            .map(|(_, s)| s.clone())
            .collect::<HashSet<_>>();

        while {
            let next = deps
                .iter()
                .filter(|(_, (cm::Target { kind, .. }, _))| *kind == ["lib".to_owned()])
                .map(|(package_id, _)| nodes[package_id])
                .flat_map(|cm::Node { deps, .. }| deps)
                .filter(|node_dep| {
                    satisfies(node_dep, false) && all_package_ids.insert(&node_dep.pkg)
                })
                .flat_map(|cm::NodeDep { pkg, .. }| {
                    let package = &self[pkg];
                    let target = package.targets.iter().find(|cm::Target { kind, .. }| {
                        *kind == ["lib".to_owned()] || *kind == ["proc-macro".to_owned()]
                    })?;
                    let mut extern_crate_name = format!(
                        "__{}_{}",
                        package.name.replace('-', "_"),
                        package
                            .version
                            .to_string()
                            .replace(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9'), "_"),
                    );
                    while !all_extern_crate_names.insert(extern_crate_name.clone()) {
                        extern_crate_name += "_";
                    }
                    Some((&package.id, (target, extern_crate_name)))
                })
                .collect::<Vec<_>>();
            let next_is_empty = next.is_empty();
            deps.extend(next);
            !next_is_empty
        } {}

        Ok(deps)
    }

    fn dep_lib_by_extern_crate_name(
        &self,
        package_id: &cm::PackageId,
        extern_crate_name: &str,
    ) -> Option<&cm::Package> {
        // https://docs.rs/cargo/0.47.0/src/cargo/core/resolver/resolve.rs.html#323-352

        let package = &self[package_id];

        let node = self
            .resolve
            .as_ref()
            .into_iter()
            .flat_map(|cm::Resolve { nodes, .. }| nodes)
            .find(|cm::Node { id, .. }| id == package_id)?;

        let found_explicitly_renamed_one = package
            .dependencies
            .iter()
            .flat_map(|cm::Dependency { rename, .. }| rename)
            .any(|rename| rename == extern_crate_name);

        if found_explicitly_renamed_one {
            Some(
                &self[&node
                    .deps
                    .iter()
                    .find(|cm::NodeDep { name, .. }| name == extern_crate_name)
                    .expect("found the dep in `dependencies`, not in `resolve.deps`")
                    .pkg],
            )
        } else {
            node.dependencies
                .iter()
                .map(|dep_id| &self[dep_id])
                .flat_map(|p| p.targets.iter().map(move |t| (t, p)))
                .find(|(t, _)| {
                    t.crate_name() == extern_crate_name
                        && (*t.kind == ["lib".to_owned()] || *t.kind == ["proc-macro".to_owned()])
                })
                .map(|(_, p)| p)
                .or_else(|| {
                    matches!(package.lib_like_target(), Some(t) if t.crate_name() == extern_crate_name)
                        .then(|| package)
                })
        }
    }

    fn libs_with_extern_crate_names(
        &self,
        package_id: &cm::PackageId,
        only: &HashSet<&cm::PackageId>,
    ) -> anyhow::Result<BTreeMap<&cm::PackageId, String>> {
        let package = &self[package_id];

        let renames = package
            .dependencies
            .iter()
            .flat_map(|cm::Dependency { rename, .. }| rename)
            .collect::<HashSet<_>>();

        let cm::Resolve { nodes, .. } =
            self.resolve.as_ref().with_context(|| "`resolve` is null")?;

        let cm::Node { deps, .. } = nodes
            .iter()
            .find(|cm::Node { id, .. }| id == package_id)
            .with_context(|| "could not find the node")?;

        Ok(deps
            .iter()
            .filter(|cm::NodeDep { pkg, dep_kinds, .. }| {
                matches!(
                    &**dep_kinds,
                    [cm::DepKindInfo {
                        kind: cm::DependencyKind::Normal,
                        ..
                    }]
                ) && only.contains(pkg)
            })
            .flat_map(|cm::NodeDep { name, pkg, .. }| {
                let extern_crate_name = if renames.contains(name) {
                    name.clone()
                } else {
                    self[pkg]
                        .targets
                        .iter()
                        .find(|cm::Target { kind, .. }| {
                            *kind == ["lib".to_owned()] || *kind == ["proc-macro".to_owned()]
                        })?
                        .crate_name()
                };
                Some((pkg, extern_crate_name))
            })
            .collect())
    }
}

fn target_by_kind_and_name<'a>(
    metadata: &'a cm::Metadata,
    kind: &str,
    name: &str,
) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
    match *targets_in_ws(metadata)
        .filter(|(t, _)| t.name == name && t.kind == [kind.to_owned()])
        .collect::<Vec<_>>()
    {
        [] => bail!("no {} target named `{}`", kind, name),
        [target] => Ok(target),
        [..] => bail!(
            "multiple {} targets named `{}` in this workspace",
            kind,
            name,
        ),
    }
}

fn targets_in_ws(metadata: &cm::Metadata) -> impl Iterator<Item = (&cm::Target, &cm::Package)> {
    metadata
        .packages
        .iter()
        .filter(move |cm::Package { id, .. }| metadata.workspace_members.contains(id))
        .flat_map(|p| p.targets.iter().map(move |t| (t, p)))
}

pub(crate) trait PackageExt {
    fn has_custom_build(&self) -> bool;
    fn has_lib(&self) -> bool;
    fn has_proc_macro(&self) -> bool;
    fn lib_like_target(&self) -> Option<&cm::Target>;
    fn manifest_dir(&self) -> &Utf8Path;
    fn edition(&self) -> Edition;
    fn read_license_text(&self, mine: &[User], cache_dir: &Path) -> anyhow::Result<Option<String>>;
}

impl PackageExt for cm::Package {
    fn has_custom_build(&self) -> bool {
        self.targets.iter().any(TargetExt::is_custom_build)
    }

    fn has_lib(&self) -> bool {
        self.targets.iter().any(TargetExt::is_lib)
    }

    fn has_proc_macro(&self) -> bool {
        self.targets.iter().any(TargetExt::is_proc_macro)
    }

    fn lib_like_target(&self) -> Option<&cm::Target> {
        self.targets.iter().find(|cm::Target { kind, .. }| {
            [&["lib".to_owned()][..], &["proc-macro".to_owned()][..]].contains(&&**kind)
        })
    }

    fn manifest_dir(&self) -> &Utf8Path {
        self.manifest_path.parent().expect("should not be empty")
    }

    fn edition(&self) -> Edition {
        self.edition.parse().expect("`edition` modified invalidly")
    }

    fn read_license_text(&self, mine: &[User], cache_dir: &Path) -> anyhow::Result<Option<String>> {
        license::read_non_unlicense_license_file(self, mine, cache_dir)
    }
}

pub(crate) trait PackageIdExt {
    fn mask_path(&self) -> String;
}

impl PackageIdExt for cm::PackageId {
    fn mask_path(&self) -> String {
        if_chain! {
            if let [s1, s2] = *self.repr.split(" (path+").collect::<Vec<_>>();
            if s2.ends_with(')');
            then {
                format!(
                    "{} (path+{})",
                    s1,
                    s2.chars().map(|_| '█').collect::<String>(),
                )
            } else {
                self.repr.clone()
            }
        }
    }
}

pub(crate) trait TargetExt {
    fn is_bin(&self) -> bool;
    fn is_example(&self) -> bool;
    fn is_custom_build(&self) -> bool;
    fn is_lib(&self) -> bool;
    fn is_proc_macro(&self) -> bool;
    fn crate_name(&self) -> String;
    fn target_option(&self) -> Vec<&str>;
}

impl TargetExt for cm::Target {
    fn is_bin(&self) -> bool {
        self.kind == ["bin".to_owned()]
    }

    fn is_example(&self) -> bool {
        self.kind == ["example".to_owned()]
    }

    fn is_custom_build(&self) -> bool {
        self.kind == ["custom-build".to_owned()]
    }

    fn is_lib(&self) -> bool {
        self.kind == ["lib".to_owned()]
    }

    fn is_proc_macro(&self) -> bool {
        self.kind == ["proc-macro".to_owned()]
    }

    fn crate_name(&self) -> String {
        self.name.replace('-', "_")
    }

    fn target_option(&self) -> Vec<&str> {
        if self.is_lib() {
            vec!["--lib"]
        } else if self.is_example() {
            vec!["--example", &self.name]
        } else {
            vec!["--bin", &self.name]
        }
    }
}

trait SourceExt {
    fn rev_git(&self) -> Option<(&str, &str)>;
}

impl SourceExt for cm::Source {
    fn rev_git(&self) -> Option<(&str, &str)> {
        let url = self.repr.strip_prefix("git+")?;
        match *url.split('#').collect::<Vec<_>>() {
            [url, rev] => Some((url, rev)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, EnumString)]
pub(crate) enum Edition {
    #[strum(serialize = "2015")]
    Edition2015,
    #[strum(serialize = "2018")]
    Edition2018,
    #[strum(serialize = "2021")]
    Edition2021,
    #[strum(serialize = "2024")]
    Edition2024,
}

impl Edition {
    fn default_resolver_behavior(self) -> ResolveBehavior {
        match self {
            Self::Edition2015 | Self::Edition2018 => ResolveBehavior::V1,
            Self::Edition2021 | Self::Edition2024 => ResolveBehavior::V2,
        }
    }
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Deserialize)]
pub(crate) enum ResolveBehavior {
    #[serde(rename = "1")]
    V1,
    #[serde(rename = "2")]
    V2,
}
