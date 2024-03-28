mod search_unused;

use crate::search_unused::analyze_package;
use anyhow::{bail, Context};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::mpsc;
use std::{fs, path::PathBuf};
use walkdir::{DirEntry, WalkDir};

#[derive(argh::FromArgs, Debug)]
#[argh(description = r#"
cargo-machete-nk: Helps find unused dependencies in a fast yet imprecise way (unless `with_metadata` flag is provided).

Exit code:
    0:  when no unused dependencies are found
    1:  when at least one unused (non-ignored) dependency is found
    2:  on error
"#)]
struct MacheteArgs {
    /// uses cargo-metadata to figure out the dependencies' names. May be useful if some
    /// dependencies are renamed from their own Cargo.toml file (e.g. xml-rs which gets renamed
    /// xml). Try it if you get false positives!
    #[argh(switch)]
    with_metadata: bool,

    /// don't analyze anything contained in any target/ directories encountered.
    #[argh(switch)]
    skip_target_dir: bool,

    /// rewrite the Cargo.toml files to automatically remove unused dependencies.
    ///
    /// Notes:
    /// - all dependencies flagged by cargo-machete-nk will be removed, including false positives.
    /// - does not fix ignored_used for now (but can).
    #[argh(switch)]
    fix: bool,

    /// print version.
    #[argh(switch)]
    version: bool,

    /// paths to directories that must be scanned.
    #[argh(option)]
    paths: Vec<PathBuf>,

    /// directories to exclude
    #[argh(option)]
    exclude: Vec<PathBuf>,

    /// preserve cache for manifest files that have not been changed.
    #[argh(option)]
    cache_path: Option<PathBuf>,
}

/// Runs `cargo-machete-nk`.
/// Returns Ok with a bool whether any unused dependencies or ignored_used were found, or Err on errors.
fn run_machete() -> anyhow::Result<bool> {
    pretty_env_logger::init();

    let args = init_args()?;
    let mut cache = init_cache(&args);

    let manifest_path_entries: Vec<PathBuf> = args
        .paths
        .clone()
        .into_iter()
        .flat_map(|path| {
            collect_paths(&path, args.skip_target_dir, &args.exclude)
                .expect("Could not analyze dependencies in {path:?}")
        })
        .collect();

    for path in manifest_path_entries {
        let current_checksum = checksum(&path);
        if let Some(cached_entry) = cache.records.get_mut(&path) {
            if cached_entry.checksum != current_checksum {
                *cached_entry = CargoDep {
                    checksum: current_checksum,
                    deps: None,
                }
            }
        } else {
            cache.records.insert(
                path,
                CargoDep {
                    checksum: current_checksum,
                    deps: None,
                },
            );
        }
    }

    log::debug!("Cache after checksum checks = {:#?}", cache);
    let unchecked_packages = cache
        .records
        .iter()
        .filter_map(|(path, dep)| dep.deps.is_none().then_some(path.to_owned()))
        .collect();
    let (to_cache_sender, analyzed_deps) = mpsc::channel();
    let cache_thread = std::thread::spawn(move || {
        while let Ok((path, deps)) = analyzed_deps.recv() {
            let cache_entry = cache.records.get_mut(&path).expect("Infallible");
            cache_entry.deps = Some(deps);
        }
        cache
    });

    analyze_packages(unchecked_packages, args.with_metadata, to_cache_sender);

    let mut cache = cache_thread.join().unwrap();
    log::debug!("Cache after all the logic ran = {:#?}", cache);

    let mut has_unused_dependencies = false;
    let mut has_ignored_used = false;
    let mut has_unused_dependencies_warning_shown = false;
    for (path, cargo_dep) in &mut cache.records {
        let Some(deps) = &mut cargo_dep.deps else {
            continue;
        };
        if deps.ignored_used.is_empty() && deps.unused.is_empty() {
            continue;
        }
        if !has_unused_dependencies_warning_shown {
            println!(
                "cargo-machete-nk found unused or ignored_used dependencies in {:?}:",
                args.paths
            );
            has_unused_dependencies_warning_shown = true;
        }
        println!("{} -- {}:", deps.package_name, path.to_string_lossy());
        for dep in &deps.unused {
            has_unused_dependencies = true;
            println!("    ❗ {dep}");
        }
        for dep in &deps.ignored_used {
            has_ignored_used = true;
            eprintln!("    ⚠️  {dep} was marked as ignored, but is actually used!");
        }
        if args.fix {
            let fixed =
                remove_dependencies(&fs::read_to_string(path)?, &deps.unused, &deps.ignored_used)?;
            fs::write(path, fixed).expect("Cargo.toml write error");
            // Save new checksum after fix and clear unused deps
            let new_checksum = checksum(path);
            cargo_dep.checksum = new_checksum;
            deps.unused = Vec::new();
            deps.ignored_used = Vec::new();
            // after fix actually there would be no unused dependencies or ignored_used
            has_unused_dependencies = false;
            has_ignored_used = false;
        }
    }

    if has_unused_dependencies {
        println!(
            "\n\
            If you believe cargo-machete-nk has detected an unused dependency incorrectly,\n\
            you can add the dependency to the list of dependencies to ignore in the\n\
            `[package.metadata.cargo-machete]` section of the appropriate Cargo.toml.\n\
            For example:\n\
            \n\
            [package.metadata.cargo-machete]\n\
            ignored = [\"prost\"]"
        );

        if !args.with_metadata {
            println!(
                "\n\
                You can also try running it with the `--with-metadata` flag for better accuracy,\n\
                though this may modify your Cargo.lock files."
            );
        }

        println!()
    }

    if !(has_unused_dependencies || has_ignored_used) {
        if args.fix {
            println!("All unused/ignored_used dependencies has been fixed!")
        } else {
            println!(
                "cargo-machete-nk didn't find any unused or ignored used dependencies. Good job!",
            );
        }
    }

    // save cache
    if let Some(cache_path) = args.cache_path {
        if let Err(e) = persist_cache(cache, cache_path) {
            eprintln!("Could not persist cache due to - {e:?}");
        }
    }

    Ok(has_unused_dependencies || has_ignored_used)
}

fn init_args() -> anyhow::Result<MacheteArgs> {
    let mut args: MacheteArgs = if running_as_cargo_cmd() {
        argh::cargo_from_env()
    } else {
        argh::from_env()
    };
    log::debug!("args = {:#?}", args);
    if args.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    if args.paths.is_empty() {
        eprintln!("Analyzing dependencies of crates in this directory...");
        args.paths.push(std::env::current_dir()?);
    } else {
        eprintln!(
            "Analyzing dependencies of crates in {}...",
            args.paths
                .iter()
                .cloned()
                .map(|path| path.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    let base_dir = std::env::current_dir()?;
    for p in &mut args.exclude {
        *p = base_dir.join(p.clone());
        if !p.exists() {
            bail!("Exclude path `{p:?}` does not exist");
        }
    }
    Ok(args)
}

fn init_cache(args: &MacheteArgs) -> Cache {
    let cache_serialized = {
        if let Some(cache_path) = &args.cache_path {
            std::fs::read_to_string(cache_path).unwrap_or_default()
        } else {
            String::new()
        }
    };

    // Stick with default if either something is wrong with the cache or it is empty
    let mut cache: Cache =
        serde_json::from_str(&cache_serialized).unwrap_or(Cache::with_paths(args.paths.clone()));
    if cache.paths != args.paths {
        // Clear cache for different paths
        // todo: we can cache per paths (make combination of paths as keys to cache.)
        cache = Cache::with_paths(args.paths.clone())
    }
    log::debug!("Initial Cache = {:#?}", cache);
    cache
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Checks whether entry is included into `into`
fn is_included(entry: &DirEntry, into: &[PathBuf]) -> bool {
    into.iter().any(|i| entry.path().starts_with(i))
}

fn collect_paths(
    path: &Path,
    skip_target_dir: bool,
    to_exclude: &[PathBuf],
) -> Result<Vec<PathBuf>, walkdir::Error> {
    let walker = WalkDir::new(path).into_iter();

    let manifest_path_entries = if skip_target_dir {
        walker
            .filter_entry(|entry| {
                !is_included(entry, to_exclude)
                    && !is_hidden(entry)
                    && !entry.path().ends_with("target")
            })
            .collect()
    } else {
        walker
            .filter_entry(|entry| !is_included(entry, to_exclude) && !is_hidden(entry))
            .collect::<Vec<_>>()
    };

    // Keep only errors and `Cargo.toml` files (filter), then map correct paths into owned
    // `PathBuf`.
    manifest_path_entries
        .into_iter()
        .filter(|entry| match entry {
            Ok(entry) => entry.file_name() == "Cargo.toml",
            Err(_) => true,
        })
        .map(|res_entry| res_entry.map(|e| e.into_path()))
        .collect()
}

/// Return true if this is run as `cargo machete`, false otherwise (`cargo-machete`, `cargo run -- ...`)
fn running_as_cargo_cmd() -> bool {
    // If run under Cargo in general, a `CARGO` environment variable is set.
    //
    // But this is also set when running with `cargo run`, which we don't want to break! In that
    // latter case, another set of cargo variables are defined, which aren't defined when just
    // running as `cargo machete`. Picked `CARGO_PKG_NAME` as one of those variables.
    //
    // So we're running under cargo if `CARGO` is defined, but not `CARGO_PKG_NAME`.
    std::env::var("CARGO").is_ok() && std::env::var("CARGO_PKG_NAME").is_err()
}

fn checksum(file: &Path) -> String {
    let bytes = fs::read(file).unwrap();
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cache {
    records: BTreeMap<PathBuf, CargoDep>,
    paths: Vec<PathBuf>,
}

impl Cache {
    pub fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            records: Default::default(),
            paths,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoDep {
    pub checksum: String,
    pub deps: Option<Deps>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deps {
    pub unused: Vec<String>,
    pub ignored_used: Vec<String>,
    pub package_name: String,
}

fn persist_cache(mut cache: Cache, cache_path: PathBuf) -> anyhow::Result<()> {
    cache.paths.sort();
    let serialized = serde_json::to_string_pretty(&cache)?;
    std::fs::write(cache_path, serialized)?;

    Ok(())
}

fn analyze_packages(
    entries: Vec<PathBuf>,
    with_metadata: bool,
    to_cache: mpsc::Sender<(PathBuf, Deps)>,
) {
    entries.par_iter().for_each(|manifest_path| {
        match analyze_package(manifest_path, with_metadata, to_cache.clone()) {
            Ok(None) => {
                log::info!(
                    "{} is a virtual manifest for a workspace",
                    manifest_path.to_string_lossy()
                );
            }

            Err(err) => {
                eprintln!("error when handling {}: {}", manifest_path.display(), err);
            }
            _ => (),
        }
    });
}

fn remove_dependencies(
    manifest: &str,
    dependencies_list: &[String],
    ignored_used: &[String],
) -> anyhow::Result<String> {
    let mut manifest = toml_edit::Document::from_str(manifest)?;

    // Fix unused
    let dependencies = manifest
        .iter_mut()
        .find_map(|(k, v)| (v.is_table_like() && k == "dependencies").then_some(Some(v)))
        .flatten()
        .context("dependencies table is missing or empty")?
        .as_table_mut()
        .context("unexpected missing table, please report with a test case on https://github.com/arsenron/cargo-machete-nk")?;

    for k in dependencies_list {
        let removed = dependencies.remove(k);
        // Cannot find a dependency, so check for `-` or `_` clash
        if removed.is_none() {
            let replaced = if k.contains('_') {
                k.replace('_', "-")
            } else if k.contains('-') {
                k.replace('-', "_")
            } else {
                bail!("Dependency {k} not found")
            };
            dependencies
                .remove(&replaced)
                .with_context(|| format!("Dependency {k} not found"))?;
        }
    }

    // Fix ignored_used
    if let Some(table) = manifest["package"]["metadata"]["cargo-machete"].as_table_mut() {
        if let Some(s) = table.get_mut("ignored") {
            let ignored = s.as_array_mut().expect("ignored MUST be an array");
            ignored.retain(|i| {
                ignored_used
                    .iter()
                    .all(|iu| iu != i.as_str().expect("Ignored elements MUST be of type string"))
            });
        }
    }

    let serialized = manifest.to_string();
    Ok(serialized)
}

fn main() -> ExitCode {
    let exit_code = match run_machete() {
        Ok(false) => 0,
        Ok(true) => 1,
        Err(err) => {
            eprintln!("Error: {err}");
            2
        }
    };

    ExitCode::from(exit_code)
}

#[cfg(test)]
const TOP_LEVEL: &str = concat!(env!("CARGO_MANIFEST_DIR"));

#[test]
fn test_ignore_target() {
    let entries = collect_paths(
        &PathBuf::from(TOP_LEVEL).join("./integration-tests/with-target/"),
        true,
        &[],
    );
    assert!(entries.unwrap().is_empty());

    let entries = collect_paths(
        &PathBuf::from(TOP_LEVEL).join("./integration-tests/with-target/"),
        false,
        &[],
    );
    assert!(!entries.unwrap().is_empty());
}
