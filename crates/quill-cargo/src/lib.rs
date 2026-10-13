use quill_cfg::CfgSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CargoWorkspace {
    pub root: PathBuf,
    pub packages: Vec<CargoPackage>,
    /// Package indices grouped so every local dependency is in an earlier wave.
    pub dependency_waves: Vec<Vec<usize>>,
    pub external_dependency_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CargoPackage {
    pub id: String,
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    pub is_workspace_member: bool,
    pub is_default_member: bool,
    pub cfg: CfgSet,
    pub targets: Vec<CargoTarget>,
    /// Resolved dependency aliases used in Rust paths, including registry deps.
    pub dependencies: Vec<CargoDependency>,
    /// IDs of all resolved normal/build dependency packages.
    pub dependency_package_ids: Vec<String>,
    /// Names of workspace packages this package depends on.
    pub local_dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CargoDependency {
    /// Identifier visible in Rust source, which may be renamed in Cargo.toml.
    pub crate_name: String,
    pub package_id: String,
    pub package_name: String,
    pub is_workspace_member: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CargoTarget {
    pub name: String,
    pub source_path: PathBuf,
    pub kinds: Vec<String>,
    pub crate_types: Vec<String>,
    pub edition: String,
    pub test: bool,
    pub doctest: bool,
}

#[derive(Debug)]
pub enum CargoError {
    MetadataCommand(String),
    InvalidMetadata(String),
    DependencyCycle(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CargoInputStamp {
    path: PathBuf,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    device: u64,
    inode: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct CargoWorkspaceCache {
    manifest: PathBuf,
    workspace_root: PathBuf,
    inputs: Vec<CargoInputStamp>,
    environment: Vec<(String, Option<String>)>,
    rustc_version: String,
    workspace: CargoWorkspace,
}

impl std::fmt::Display for CargoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MetadataCommand(message) | Self::InvalidMetadata(message) => f.write_str(message),
            Self::DependencyCycle(packages) => {
                write!(
                    f,
                    "workspace package dependency cycle: {}",
                    packages.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CargoError {}

/// Ask Cargo to resolve the manifest, then construct a deterministic graph for
/// the workspace default members and their transitive normal/build dependency
/// closure. Cargo remains authoritative for manifest syntax, target discovery,
/// feature resolution, and dependency IDs.
pub fn load_workspace(manifest_path: impl AsRef<Path>) -> Result<CargoWorkspace, CargoError> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version=1")
        .arg("--manifest-path")
        .arg(manifest_path.as_ref())
        .output()
        .map_err(|error| {
            CargoError::MetadataCommand(format!("could not run cargo metadata: {error}"))
        })?;
    if !output.status.success() {
        return Err(CargoError::MetadataCommand(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let json: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        CargoError::InvalidMetadata(format!("invalid cargo metadata JSON: {error}"))
    })?;
    let cfg = host_cfg()?;
    parse_metadata_with_cfg(&json, &cfg)
}

/// Load Cargo's resolved workspace graph from a validated local cache when the
/// workspace has a lockfile. Source edits do not affect package resolution,
/// while manifests, configuration, environment, and the active rustc do.
pub fn load_workspace_cached(
    manifest_path: impl AsRef<Path>,
    cache_root: impl AsRef<Path>,
) -> Result<CargoWorkspace, CargoError> {
    let manifest = manifest_path.as_ref();
    let Ok(manifest) = std::fs::canonicalize(manifest) else {
        return load_workspace(manifest);
    };
    let Some(root) = workspace_root(&manifest) else {
        return load_workspace(&manifest);
    };
    if !root.join("Cargo.lock").is_file() {
        return load_workspace(&manifest);
    }
    let cache_path = workspace_cache_path(cache_root.as_ref(), &manifest);
    let environment = cargo_metadata_environment();
    let rustc_version = rustc_version()?;
    if let Ok(bytes) = std::fs::read(&cache_path)
        && let Ok(cache) = serde_json::from_slice::<CargoWorkspaceCache>(&bytes)
        && cache.manifest == manifest
        && cache.workspace_root == root
        && cache.environment == environment
        && cache.rustc_version == rustc_version
        && cache.workspace.root == root
    {
        let package_manifests = cache
            .workspace
            .packages
            .iter()
            .map(|package| package.manifest_path.clone())
            .collect::<Vec<_>>();
        let current_inputs = stamp_inputs(&cargo_metadata_inputs(&root, &package_manifests));
        if current_inputs.as_ref() == Some(&cache.inputs) {
            return Ok(cache.workspace);
        }
    }

    let workspace = load_workspace(&manifest)?;
    let package_manifests = workspace
        .packages
        .iter()
        .map(|package| package.manifest_path.clone())
        .collect::<Vec<_>>();
    let inputs = stamp_inputs(&cargo_metadata_inputs(&root, &package_manifests));
    if let Some(inputs) = inputs {
        let cache = CargoWorkspaceCache {
            manifest,
            workspace_root: root,
            inputs,
            environment,
            rustc_version,
            workspace: workspace.clone(),
        };
        write_workspace_cache(&cache_path, &cache);
    }
    Ok(workspace)
}

fn workspace_root(manifest: &Path) -> Option<PathBuf> {
    for directory in manifest.parent()?.ancestors() {
        let candidate = directory.join("Cargo.toml");
        if let Ok(contents) = std::fs::read_to_string(candidate)
            && contents.lines().any(|line| line.trim() == "[workspace]")
        {
            return Some(directory.to_path_buf());
        }
    }
    None
}

fn workspace_cache_path(cache_root: &Path, manifest: &Path) -> PathBuf {
    let digest = blake3::hash(manifest.as_os_str().as_encoded_bytes());
    cache_root
        .join("cargo-workspaces")
        .join(format!("{}.json", digest.to_hex()))
}

fn cargo_metadata_environment() -> Vec<(String, Option<String>)> {
    [
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_NET_OFFLINE",
        "CARGO_NET_RETRY",
        "CARGO_REGISTRIES_CRATES_IO_PROTOCOL",
        "RUSTC",
        "RUSTFLAGS",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
        )
    })
    .collect()
}

fn rustc_version() -> Result<String, CargoError> {
    if let Some(compiler) = std::env::var_os("RUSTC") {
        let compiler = PathBuf::from(compiler);
        if let Ok(metadata) = std::fs::metadata(&compiler) {
            return Ok(format!(
                "custom-rustc:{}:{}:{}:{}:{}",
                compiler.display(),
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.ctime_nsec()
            ));
        }
    }
    if let Some(channel) = pinned_toolchain_channel() {
        return Ok(format!("rustup-pinned:{channel}"));
    }
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Err(CargoError::MetadataCommand(format!(
                "could not run rustc --version: {error}"
            )));
        }
    };
    if !output.status.success() {
        return Err(CargoError::MetadataCommand(format!(
            "rustc --version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn pinned_toolchain_channel() -> Option<String> {
    if let Some(channel) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        let channel = channel.to_string_lossy().into_owned();
        if is_pinned_toolchain(&channel) {
            return Some(channel);
        }
    }
    let current = std::env::current_dir().ok()?;
    for directory in current.ancestors() {
        let legacy = directory.join("rust-toolchain");
        if let Ok(channel) = std::fs::read_to_string(legacy) {
            let channel = channel.trim();
            if is_pinned_toolchain(channel) {
                return Some(channel.to_owned());
            }
        }
        let config = directory.join("rust-toolchain.toml");
        if let Ok(contents) = std::fs::read_to_string(config)
            && let Some(channel) = contents.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "channel")
                    .then(|| value.trim().trim_matches('"').trim_matches('\'').to_owned())
            })
            && is_pinned_toolchain(&channel)
        {
            return Some(channel);
        }
    }
    None
}

fn is_pinned_toolchain(channel: &str) -> bool {
    let Some(date) = channel.strip_prefix("nightly-") else {
        return false;
    };
    date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn cargo_metadata_inputs(root: &Path, package_manifests: &[PathBuf]) -> Vec<PathBuf> {
    fn visit(directory: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                    visit(&path, paths);
                }
            } else if file_type.is_file() {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some("Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml")
                ) || (path
                    .parent()
                    .is_some_and(|parent| parent.ends_with(".cargo"))
                    && matches!(name.to_str(), Some("config" | "config.toml")))
                {
                    paths.push(path);
                }
            }
        }
    }

    let mut paths = Vec::new();
    visit(root, &mut paths);
    paths.extend(package_manifests.iter().cloned());
    for search_root in [Some(root.to_path_buf()), std::env::current_dir().ok()]
        .into_iter()
        .flatten()
    {
        for ancestor in search_root.ancestors() {
            for name in ["rust-toolchain.toml", "rust-toolchain"] {
                let path = ancestor.join(name);
                if path.is_file() {
                    paths.push(path);
                }
            }
            for name in ["config.toml", "config"] {
                let path = ancestor.join(".cargo").join(name);
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        for name in ["config.toml", "config"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn stamp_inputs(paths: &[PathBuf]) -> Option<Vec<CargoInputStamp>> {
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).ok()?;
            Some(CargoInputStamp {
                path: path.clone(),
                size: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        })
        .collect()
}

fn write_workspace_cache(path: &Path, cache: &CargoWorkspaceCache) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

fn host_cfg() -> Result<CfgSet, CargoError> {
    let mut command = Command::new("rustc");
    command.args(["--print", "cfg"]);
    if let Some(target) = std::env::var_os("CARGO_BUILD_TARGET") {
        command.arg("--target").arg(target);
    }
    let output = command.output().map_err(|error| {
        CargoError::MetadataCommand(format!("could not run rustc --print cfg: {error}"))
    })?;
    if !output.status.success() {
        return Err(CargoError::MetadataCommand(format!(
            "rustc --print cfg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(CfgSet::parse_rustc_cfg(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Convert Cargo metadata JSON to a graph. Kept independent of process launch
/// so graph invariants can be tested with small metadata fixtures.
pub fn parse_metadata(metadata: &Value) -> Result<CargoWorkspace, CargoError> {
    parse_metadata_with_cfg(metadata, &CfgSet::default())
}

pub fn parse_metadata_with_cfg(
    metadata: &Value,
    target_cfg: &CfgSet,
) -> Result<CargoWorkspace, CargoError> {
    let workspace_root = string(metadata, "workspace_root")?;
    let member_ids = string_array(metadata, "workspace_default_members")?;
    let workspace_member_ids = string_array(metadata, "workspace_members")?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("missing packages array"))?;
    let selected: BTreeSet<_> = member_ids.into_iter().collect();
    let mut package_by_id = HashMap::new();
    for package in packages {
        package_by_id.insert(string(package, "id")?, package);
    }

    let resolve_nodes = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("metadata has no resolved dependency graph"))?;
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in resolve_nodes {
        let id = string(node, "id")?;
        let mut deps = BTreeSet::new();
        if let Some(dependencies) = node.get("deps").and_then(Value::as_array) {
            for dependency in dependencies {
                if !is_build_dependency(dependency) {
                    continue;
                }
                let dep_id = string(dependency, "pkg")?;
                deps.insert(dep_id);
            }
        } else if let Some(dependencies) = node.get("dependencies").and_then(Value::as_array) {
            for dependency in dependencies {
                let dep_id = dependency
                    .as_str()
                    .ok_or_else(|| invalid("dependency id is not a string"))?;
                deps.insert(dep_id.to_owned());
            }
        }
        edges.insert(id, deps);
    }

    let mut reachable = selected.clone();
    let mut pending: Vec<_> = selected.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        for dependency in edges.get(&id).into_iter().flatten() {
            if !package_by_id.contains_key(dependency) {
                return Err(invalid("resolved dependency missing from packages"));
            }
            if reachable.insert(dependency.clone()) {
                pending.push(dependency.clone());
            }
        }
    }
    let external_dependency_count = reachable
        .iter()
        .flat_map(|id| edges.get(id).into_iter().flatten())
        .filter(|id| !workspace_member_ids.contains(id))
        .count();

    let mut ids: Vec<_> = reachable.into_iter().collect();
    ids.sort_by_key(|id| {
        (
            package_by_id
                .get(id)
                .and_then(|package| package.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned(),
            id.clone(),
        )
    });
    let mut packages_out = Vec::with_capacity(ids.len());
    let mut index_by_id = HashMap::new();
    for id in &ids {
        let package = package_by_id
            .get(id)
            .ok_or_else(|| invalid("workspace member missing from packages"))?;
        let name = string(package, "name")?;
        let targets = package
            .get("targets")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("package missing targets"))?
            .iter()
            .map(parse_target)
            .collect::<Result<Vec<_>, _>>()?;
        let dependency_ids: Vec<_> = edges.get(id).into_iter().flatten().cloned().collect();
        let local_dependencies = dependency_ids
            .iter()
            .filter(|dep| workspace_member_ids.contains(dep))
            .filter_map(|dep| {
                package_by_id
                    .get(dep)
                    .and_then(|package| package.get("name"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let dependencies =
            node_dependencies(resolve_nodes, id, &package_by_id, &workspace_member_ids)?;
        let mut cfg = target_cfg.clone();
        for feature in node_features(resolve_nodes, id)? {
            cfg.insert_value("feature", feature);
        }
        index_by_id.insert(id.clone(), packages_out.len());
        packages_out.push(CargoPackage {
            id: id.clone(),
            name,
            version: string(package, "version")?,
            manifest_path: PathBuf::from(string(package, "manifest_path")?),
            is_workspace_member: workspace_member_ids.contains(id),
            is_default_member: selected.contains(id),
            cfg,
            targets,
            dependencies,
            dependency_package_ids: dependency_ids,
            local_dependencies,
        });
    }

    let mut remaining: BTreeSet<_> = ids.iter().cloned().collect();
    let mut dependency_waves = Vec::new();
    while !remaining.is_empty() {
        let wave_ids: Vec<_> = remaining
            .iter()
            .filter(|id| {
                edges
                    .get(*id)
                    .is_none_or(|deps| deps.iter().all(|dep| !remaining.contains(dep)))
            })
            .cloned()
            .collect();
        if wave_ids.is_empty() {
            let names = remaining
                .iter()
                .filter_map(|id| {
                    package_by_id
                        .get(id)
                        .and_then(|package| package.get("name"))
                        .and_then(Value::as_str)
                })
                .map(str::to_owned)
                .collect();
            return Err(CargoError::DependencyCycle(names));
        }
        let mut wave: Vec<_> = wave_ids
            .iter()
            .filter_map(|id| index_by_id.get(id).copied())
            .collect();
        wave.sort_by_key(|index| packages_out[*index].name.clone());
        dependency_waves.push(wave);
        for id in wave_ids {
            remaining.remove(&id);
        }
    }

    Ok(CargoWorkspace {
        root: PathBuf::from(workspace_root),
        packages: packages_out,
        dependency_waves,
        external_dependency_count,
    })
}

fn node_features(nodes: &[Value], id: &str) -> Result<Vec<String>, CargoError> {
    let Some(node) = nodes
        .iter()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Ok(Vec::new());
    };
    match node.get("features") {
        Some(_) => string_array(node, "features"),
        None => Ok(Vec::new()),
    }
}

fn parse_target(target: &Value) -> Result<CargoTarget, CargoError> {
    Ok(CargoTarget {
        name: string(target, "name")?,
        source_path: PathBuf::from(string(target, "src_path")?),
        kinds: string_array(target, "kind")?,
        crate_types: string_array(target, "crate_types")?,
        edition: string(target, "edition")?,
        test: target.get("test").and_then(Value::as_bool).unwrap_or(false),
        doctest: target
            .get("doctest")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn node_dependencies(
    nodes: &[Value],
    id: &str,
    package_by_id: &HashMap<String, &Value>,
    workspace_members: &[String],
) -> Result<Vec<CargoDependency>, CargoError> {
    let Some(node) = nodes
        .iter()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Ok(Vec::new());
    };
    let mut dependencies = BTreeMap::new();
    if let Some(deps) = node.get("deps").and_then(Value::as_array) {
        for dependency in deps {
            if !is_build_dependency(dependency) {
                continue;
            }
            let dep_id = string(dependency, "pkg")?;
            let package = package_by_id
                .get(&dep_id)
                .ok_or_else(|| invalid("resolved dependency missing from packages"))?;
            let package_name = string(package, "name")?;
            let crate_name = dependency
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&package_name)
                .to_owned();
            dependencies.insert(
                (crate_name.clone(), dep_id.clone()),
                CargoDependency {
                    crate_name,
                    package_id: dep_id.clone(),
                    package_name,
                    is_workspace_member: workspace_members.contains(&dep_id),
                },
            );
        }
    } else if let Some(deps) = node.get("dependencies").and_then(Value::as_array) {
        for dependency in deps {
            let package_id = dependency
                .as_str()
                .ok_or_else(|| invalid("dependency id is not a string"))?;
            let package = package_by_id
                .get(package_id)
                .ok_or_else(|| invalid("resolved dependency missing from packages"))?;
            let package_name = string(package, "name")?;
            let crate_name = package_name.replace('-', "_");
            dependencies.insert(
                (crate_name.clone(), package_id.to_owned()),
                CargoDependency {
                    crate_name,
                    package_id: package_id.to_owned(),
                    package_name,
                    is_workspace_member: workspace_members.contains(&package_id.to_owned()),
                },
            );
        }
    }
    Ok(dependencies.into_values().collect())
}

/// Cargo's resolved node lists dependency kinds. `null` is a normal
/// dependency; build scripts and proc macros are also needed for a build.
/// Dev dependencies do not belong in `cargo build`'s package graph.
fn is_build_dependency(dependency: &Value) -> bool {
    let Some(kinds) = dependency.get("dep_kinds").and_then(Value::as_array) else {
        // Older metadata formats used a flat list of resolved package IDs.
        return true;
    };
    kinds.iter().any(|kind| {
        kind.get("kind")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "build")
    })
}

fn string(value: &Value, field: &str) -> Result<String, CargoError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid(&format!("missing string field {field}")))
}

fn string_array(value: &Value, field: &str) -> Result<Vec<String>, CargoError> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(&format!("missing array field {field}")))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(&format!("non-string entry in {field}")))
        })
        .collect()
}

fn invalid(message: &str) -> CargoError {
    CargoError::InvalidMetadata(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{load_workspace_cached, parse_metadata};
    use serde_json::json;
    use std::path::PathBuf;

    fn target(name: &str, src_path: &str, kind: &str) -> serde_json::Value {
        json!({"name": name, "src_path": src_path, "kind": [kind], "crate_types": [kind], "edition": "2024", "test": true, "doctest": false})
    }

    #[test]
    fn closes_transitive_dependencies_and_orders_dependency_waves() {
        let metadata = json!({
            "workspace_root": "/workspace",
            "workspace_default_members": ["app-id"],
            "workspace_members": ["app-id", "core-id", "unused-id"],
            "packages": [
                {"id":"app-id", "name":"app", "version":"0.1.0", "manifest_path":"/workspace/app/Cargo.toml", "targets":[target("app", "/workspace/app/src/main.rs", "bin")]},
                {"id":"core-id", "name":"core", "version":"0.1.0", "manifest_path":"/workspace/core/Cargo.toml", "targets":[target("core", "/workspace/core/src/lib.rs", "lib")]},
                {"id":"unused-id", "name":"unused", "version":"0.1.0", "manifest_path":"/workspace/unused/Cargo.toml", "targets":[target("unused", "/workspace/unused/src/lib.rs", "lib")]},
                {"id":"registry-id", "name":"serde", "version":"1.0.0", "manifest_path":"/registry/serde/Cargo.toml", "targets":[]}
            ],
            "resolve":{"nodes":[
                {"id":"app-id", "features":["fast-path"], "deps":[
                    {"name":"core_alias", "pkg":"core-id", "dep_kinds":[{"kind":null,"target":null}]},
                    {"name":"unused", "pkg":"unused-id", "dep_kinds":[{"kind":"dev","target":null}]}
                ]},
                {"id":"core-id", "deps":[{"name":"serde", "pkg":"registry-id", "dep_kinds":[{"kind":null,"target":null}]}]},
                {"id":"unused-id", "deps":[]},
                {"id":"registry-id", "deps":[]}
            ]}
        });
        let graph = parse_metadata(&metadata).unwrap();
        assert_eq!(
            graph
                .packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["app", "core", "serde"]
        );
        assert_eq!(graph.dependency_waves, [vec![2], vec![1], vec![0]]);
        assert!(graph.packages[0].is_default_member);
        assert!(graph.packages[0].cfg.has_value("feature", "fast-path"));
        assert!(!graph.packages[1].is_default_member);
        assert!(!graph.packages[2].is_workspace_member);
        assert_eq!(graph.packages[0].local_dependencies, ["core"]);
        assert_eq!(graph.packages[0].dependencies[0].crate_name, "core_alias");
        assert_eq!(graph.packages[0].dependencies[0].package_name, "core");
        assert!(graph.packages[0].dependencies[0].is_workspace_member);
        assert_eq!(graph.packages[1].dependencies[0].crate_name, "serde");
        assert!(!graph.packages[1].dependencies[0].is_workspace_member);
        assert_eq!(graph.external_dependency_count, 1);
        assert_eq!(
            graph.packages[0].targets[0].source_path,
            PathBuf::from("/workspace/app/src/main.rs")
        );
    }

    #[test]
    fn rejects_local_dependency_cycles() {
        let metadata = json!({
            "workspace_root":"/w", "workspace_default_members":["a", "b"], "workspace_members":["a", "b"],
            "packages":[
                {"id":"a", "name":"a", "version":"0.1.0", "manifest_path":"/w/a/Cargo.toml", "targets":[]},
                {"id":"b", "name":"b", "version":"0.1.0", "manifest_path":"/w/b/Cargo.toml", "targets":[]}
            ],
            "resolve":{"nodes":[{"id":"a", "deps":[{"pkg":"b"}]}, {"id":"b", "deps":[{"pkg":"a"}]}]}
        });
        assert!(
            parse_metadata(&metadata)
                .unwrap_err()
                .to_string()
                .contains("dependency cycle")
        );
    }

    #[test]
    fn only_dated_nightly_channels_are_treated_as_immutable_pins() {
        assert!(super::is_pinned_toolchain("nightly-2026-08-23"));
        assert!(!super::is_pinned_toolchain("nightly"));
        assert!(!super::is_pinned_toolchain("nightly-2026-8-23"));
        assert!(!super::is_pinned_toolchain("stable"));
    }

    #[test]
    fn cached_workspace_reuses_graph_and_invalidates_on_manifest_edits() {
        let root = std::env::temp_dir().join(format!(
            "quill-cargo-workspace-cache-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let app = root.join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        let manifest = app.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"cache-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(app.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        let cache = root.join("cache");

        let first = load_workspace_cached(&manifest, &cache).unwrap();
        assert_eq!(first.packages[0].version, "0.1.0");
        let second = load_workspace_cached(&manifest, &cache).unwrap();
        assert_eq!(second, first);

        std::fs::write(
            &manifest,
            "[package]\nname = \"cache-app\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let updated = load_workspace_cached(&manifest, &cache).unwrap();
        assert_eq!(updated.packages[0].version, "0.2.0");
        std::fs::remove_dir_all(root).unwrap();
    }
}
