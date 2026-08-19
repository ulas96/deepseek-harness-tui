//! Resolution of the DeepSeek Harness SDK runtime launch spec from a harness
//! checkout: source mode boots the jsonrpc-demo bin through the tsx loader,
//! built mode boots its built lib under plain Node. Mirrors
//! '@deepseek-ai/dsh-loader-smoke' resolveExampleLaunch.
//!
//! Runtime-side config discovery (unchanged by tub): '$DSH_CORDIS_CONFIG' env
//! first, else positional argv[2]; no default, no fallback. tub passes its
//! config path positionally.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which artifact the runtime bin is booted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    /// Unbuilt 'src' via tsx (zero-build dev path; TSX_TSCONFIG_PATH set).
    #[default]
    Src,
    /// Built 'lib' under plain Node.
    Lib,
}

impl RuntimeMode {
    /// Parse from the DSH_EXAMPLE_MODE-style env value ('src' or 'lib').
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "" | "src" => Ok(RuntimeMode::Src),
            "lib" => Ok(RuntimeMode::Lib),
            other => Err(format!(
                "runtime mode must be 'src' or 'lib', got {other:?}"
            )),
        }
    }
}

/// A resolved runtime launch: command + args + env overrides.
#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    pub command: String,
    pub args: Vec<String>,
    /// Environment entries layered over the parent environment.
    pub env: Vec<(String, String)>,
}

/// Resolve how to spawn the jsonrpc-demo runtime bin from a harness checkout.
///
/// The checkout must be a DeepSeek Harness repository (with node_modules
/// installed for src mode, or a built lib for lib mode).
pub fn resolve_launch(checkout: &Path, mode: RuntimeMode) -> Result<ResolvedLaunch, String> {
    let src_bin = checkout.join("packages/examples/jsonrpc-demo/src/bin.ts");
    let lib_bin = checkout.join("packages/examples/jsonrpc-demo/lib/bin.js");
    match mode {
        RuntimeMode::Src => {
            if !src_bin.is_file() {
                return Err(format!(
                    "src-mode runtime bin not found: {}",
                    src_bin.display()
                ));
            }
            let loader = checkout.join("node_modules/tsx/dist/loader.mjs");
            if !loader.is_file() {
                return Err(format!(
                    "tsx loader not found (run pnpm install in the checkout): {}",
                    loader.display()
                ));
            }
            let node = node_command()?;
            Ok(ResolvedLaunch {
                command: node,
                args: vec![
                    "--import".to_string(),
                    loader.to_string_lossy().to_string(),
                    src_bin.to_string_lossy().to_string(),
                ],
                env: vec![(
                    "TSX_TSCONFIG_PATH".to_string(),
                    checkout.join("tsconfig.json").to_string_lossy().to_string(),
                )],
            })
        }
        RuntimeMode::Lib => {
            if !lib_bin.is_file() {
                return Err(format!(
                    "lib-mode runtime bin not found (run pnpm run build in the checkout): {}",
                    lib_bin.display()
                ));
            }
            let node = node_command()?;
            Ok(ResolvedLaunch {
                command: node,
                args: vec![lib_bin.to_string_lossy().to_string()],
                env: Vec::new(),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NodeVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl NodeVersion {
    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.trim().trim_start_matches('v').split('.');
        Some(Self {
            major: numeric_prefix(parts.next()?)?,
            minor: numeric_prefix(parts.next()?)?,
            patch: numeric_prefix(parts.next().unwrap_or("0"))?,
        })
    }

    fn is_supported(self) -> bool {
        (self.major == 22 && self.minor >= 19) || self.major >= 24
    }
}

fn numeric_prefix(raw: &str) -> Option<u64> {
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn command_node_version(command: &str) -> Option<NodeVersion> {
    let output = Command::new(command).arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| NodeVersion::parse(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

/// Resolve a supported Node executable. The shell's `node` wins when it is
/// supported; otherwise tub searches installed NVM and Homebrew Node versions.
/// `TUB_NODE` is an explicit override and is validated rather than silently
/// falling back.
pub fn node_command() -> Result<String, String> {
    if let Some(explicit) = std::env::var_os("TUB_NODE") {
        let explicit = explicit.to_string_lossy().to_string();
        return supported_node(&explicit).map(|_| explicit).ok_or_else(|| {
            "TUB_NODE does not point to a supported Node runtime (need ^22.19 or >=24)".to_string()
        });
    }

    if supported_node("node").is_some() {
        return Ok("node".to_string());
    }

    let mut candidates = Vec::new();
    if let Some(nvm_bin) = std::env::var_os("NVM_BIN") {
        candidates.push(PathBuf::from(nvm_bin).join("node"));
    }

    let nvm_dir = std::env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".nvm")));
    if let Some(nvm_dir) = nvm_dir {
        let versions = nvm_dir.join("versions/node");
        if let Ok(entries) = std::fs::read_dir(versions) {
            candidates.extend(entries.flatten().map(|entry| entry.path().join("bin/node")));
        }
    }

    candidates.extend([
        PathBuf::from("/opt/homebrew/opt/node@24/bin/node"),
        PathBuf::from("/usr/local/opt/node@24/bin/node"),
    ]);

    let mut seen = HashSet::new();
    let mut supported: Vec<(NodeVersion, String)> = candidates
        .into_iter()
        .filter(|path| path.is_file() && seen.insert(path.clone()))
        .filter_map(|path| {
            let command = path.to_string_lossy().to_string();
            supported_node(&command).map(|version| (version, command))
        })
        .collect();
    supported.sort_by(|left, right| right.0.cmp(&left.0));
    supported
        .into_iter()
        .next()
        .map(|(_, command)| command)
        .ok_or_else(|| {
            "no supported Node runtime found (need ^22.19 or >=24); install Node 24 or set TUB_NODE"
                .to_string()
        })
}

fn supported_node(command: &str) -> Option<NodeVersion> {
    command_node_version(command).filter(|version| version.is_supported())
}

/// Resolve a path to absolute, failing when it does not exist.
pub fn require_absolute(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !absolute.exists() {
        return Err(format!("path does not exist: {}", absolute.display()));
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_version_support_matches_harness_requirement() {
        assert!(!NodeVersion::parse("v22.18.0").unwrap().is_supported());
        assert!(NodeVersion::parse("v22.19.0").unwrap().is_supported());
        assert!(!NodeVersion::parse("v23.11.0").unwrap().is_supported());
        assert!(NodeVersion::parse("v24.0.1").unwrap().is_supported());
        assert!(NodeVersion::parse("v25.2.0").unwrap().is_supported());
    }

    #[test]
    fn node_version_parser_accepts_prerelease_suffixes() {
        assert_eq!(
            NodeVersion::parse("v24.1.2-nightly"),
            Some(NodeVersion {
                major: 24,
                minor: 1,
                patch: 2,
            })
        );
    }
}
