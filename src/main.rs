mod search_unused;

use crate::search_unused::analyze_package;
use anyhow::{bail, Context};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
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

    /// persist file db for manifest files that have not been changed.
    #[argh(option)]
    db_path: Option<PathBuf>,
}

/// Runs `cargo-machete-nk`.
/// Returns Ok with a bool whether any unused dependencies or ignored_used were found, or Err on errors.
fn run_machete() -> anyhow::Result<bool> {
    pretty_env_logger::init();

    let args = init_args()?;
    let mut db = init_db(&args);

    let manifest_path_entries: Vec<PathBuf> = args
        .paths
        .clone()
        .into_iter()
        .flat_map(|path| {
            collect_paths(&path, args.skip_target_dir, &args.exclude)
                .expect("Could not analyze dependencies in {path:?}")
        })
        .collect();

    let unique_run_id = uuid::Uuid::new_v4().to_string();
    for cargo_toml_path in manifest_path_entries {
        if let Some(cargo_dep) = db.records.get_mut(&cargo_toml_path) {
            (*cargo_dep).unique_run_id = unique_run_id.clone();
            let current_checksum = checksum(&cargo_toml_path)?;
            if cargo_dep.checksum != current_checksum {
                (*cargo_dep).checksum = current_checksum;
                (*cargo_dep).deps = None;
            }
        } else {
            // Caculate checksum only if `db_path` is provided
            let current_checksum = if args.db_path.is_some() {
                checksum(&cargo_toml_path)?
            } else {
                String::new()
            };
            db.records.insert(
                cargo_toml_path,
                CargoDep {
                    checksum: current_checksum,
                    deps: None,
                    unique_run_id: unique_run_id.clone(),
                },
            );
        }
    }

    // Invalidate cache
    db.records.retain(|_, v| v.unique_run_id == unique_run_id);

    log::debug!("Db after checksum checks = {:#?}", db);
    let unchecked_packages = db
        .records
        .iter()
        .filter_map(|(path, dep)| dep.deps.is_none().then_some(path.to_owned()))
        .collect();
    log::debug!("Packages to check - {:?}", unchecked_packages);
    let (to_db_sender, analyzed_deps) = mpsc::channel();
    let db_thread = std::thread::spawn(move || {
        while let Ok((path, deps)) = analyzed_deps.recv() {
            let cargo_dep = db.records.get_mut(&path).expect("Infallible");
            cargo_dep.deps = Some(deps);
        }
        db
    });

    analyze_packages(unchecked_packages, args.with_metadata, to_db_sender);

    let mut db = db_thread.join().unwrap();
    log::debug!("Db after all the logic ran = {:#?}", db);

    let mut has_unused_dependencies = false;
    let mut has_ignored_used = false;
    let mut has_unused_dependencies_warning_shown = false;
    for (path, cargo_dep) in &mut db.records {
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
            let new_checksum = checksum(path)?;
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

    // persist db
    if let Some(db_path) = args.db_path {
        if let Err(e) = persist_db(db, db_path) {
            eprintln!("Could not persist db due to - {e:?}");
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

fn init_db(args: &MacheteArgs) -> Db {
    let db_serialized = {
        if let Some(db_path) = &args.db_path {
            std::fs::read_to_string(db_path).unwrap_or_default()
        } else {
            String::new()
        }
    };

    // Stick with default if either something is wrong with the db or it is empty
    let mut db: Db =
        serde_json::from_str(&db_serialized).unwrap_or(Db::with_paths(args.paths.clone()));
    if db.paths != args.paths {
        // Clear db for different paths
        // todo: we can persist dbpersist db per paths (make combination of paths as keys)
        db = Db::with_paths(args.paths.clone())
    }
    log::debug!("Initial db = {:#?}", db);
    db
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

/// Checksums recursively source files in a dir containing Cargo toml file
/// and the toml file itself
fn checksum(cargo_toml_path: &Path) -> anyhow::Result<String> {
    let cargo_dir = cargo_toml_path
        .parent()
        .expect("path not to be empty or a root");
    let src_dir = cargo_dir.join("src");
    // todo: this should be handled another way (Alter return type for example).
    //  These are probably virtual manifests (no package table) and we can skip it.
    if !src_dir.exists() {
        return Ok(String::new());
    }
    let src_tree = merkle_hash::MerkleTree::builder(
        src_dir.to_str().expect("Only UTF-8 systems are supported"),
    )
    .algorithm(merkle_hash::Algorithm::Blake3)
    .hash_names(true)
    .build()?;

    let toml_contents = fs::read(cargo_toml_path).unwrap();
    let toml_hashed = merkle_hash::blake3::hash(&toml_contents);

    let mut final_checksum = src_tree.root.item.hash;
    final_checksum.extend(&toml_hashed.as_bytes()[..]);

    Ok(hex::encode(
        merkle_hash::blake3::hash(&final_checksum).as_bytes(),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Db {
    records: BTreeMap<PathBuf, CargoDep>,
    paths: Vec<PathBuf>,
}

impl Db {
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
    /// Used to invalidate cache
    pub unique_run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deps {
    pub unused: Vec<String>,
    pub ignored_used: Vec<String>,
    pub package_name: String,
}

fn persist_db(mut db: Db, db_path: PathBuf) -> anyhow::Result<()> {
    db.paths.sort();
    let serialized = serde_json::to_string_pretty(&db)?;
    std::fs::write(db_path, serialized)?;

    Ok(())
}

fn analyze_packages(
    entries: Vec<PathBuf>,
    with_metadata: bool,
    to_db: mpsc::Sender<(PathBuf, Deps)>,
) {
    entries.par_iter().for_each(|manifest_path| {
        match analyze_package(manifest_path, with_metadata, to_db.clone()) {
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
