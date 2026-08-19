//! Resolution of the DeepSeek Harness SDK runtime launch spec from a harness
//! checkout: source mode boots the jsonrpc-demo bin through the tsx loader,
//! built mode boots its built lib under plain Node. Mirrors
//! '@deepseek-ai/dsh-loader-smoke' resolveExampleLaunch.
//!
//! Runtime-side config discovery (unchanged by tub): '$DSH_CORDIS_CONFIG' env
//! first, else positional argv[2]; no default, no fallback. tub passes its
//! config path positionally.

use std::path::{Path, PathBuf};

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
            Ok(ResolvedLaunch {
                command: "node".to_string(),
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
            Ok(ResolvedLaunch {
                command: "node".to_string(),
                args: vec![lib_bin.to_string_lossy().to_string()],
                env: Vec::new(),
            })
        }
    }
}

/// The Node executable, when discoverable on PATH.
pub fn node_command() -> String {
    "node".to_string()
}

/// Resolve a path to absolute, failing when it does not exist.
pub fn require_absolute(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !absolute.exists() {
        return Err(format!("path does not exist: {}", absolute.display()));
    }
    Ok(absolute)
}
